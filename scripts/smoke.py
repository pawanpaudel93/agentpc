#!/usr/bin/env python3
"""Smoke test: drive fresh VMs through agentpc's MCP server the way an agent does.

Run after changing an image, a guest script or the desktop driver:

    cargo build --release
    scripts/smoke.py --bin target/release/agentpc            # ubuntu and windows
    scripts/smoke.py --bin target/release/agentpc --os ubuntu
    scripts/smoke.py --bin target/release/agentpc --os x86apps  # ubuntu-x86apps (not a default)
    scripts/smoke.py --bin target/release/agentpc --os arch     # Arch Linux ARM (not a default)
    scripts/smoke.py --bin target/release/agentpc --os arch-x86apps  # (not a default)

Each OS gets its own throwaway VM (deleted afterwards). Checks that the guest isn't blocked
by first-run dialogs, that the desktop tools read and type, and that the gateway explains
bad arguments. On ubuntu and arch it also walks the lifecycle an agent uses: background
jobs, timeouts, file copies, port forwards, checkpoints, reset, and (in two more short-lived
VMs) offline mode and a non-default size. Needs internet in the guest (example.com). Exits 1 if any check fails.
Standard library only.
"""

import argparse
import hashlib
import json
import os
import platform
import queue
import re
import shutil
import socket
import subprocess
import sys
import tempfile
import threading
import time
import urllib.request

CALL_TIMEOUT = 900  # create_vm of a Windows image that needs a snapshot can take minutes


class Mcp:
    def __init__(self, binary):
        self.binary = binary
        self.proc = subprocess.Popen(
            [binary, "mcp"],
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.DEVNULL,
        )
        # A reader thread hands each line over a queue, so no reply sits unseen in a buffer
        # (select() on a buffered pipe misses a second line already read into it).
        self.lines = queue.Queue()
        threading.Thread(target=self.read_lines, daemon=True).start()
        self.next_id = 0
        self.calls = 0
        self.request(
            "initialize",
            {
                "protocolVersion": "2025-06-18",
                "capabilities": {},
                "clientInfo": {"name": "agentpc-smoke", "version": "1"},
            },
        )
        self.notify("notifications/initialized")

    def read_lines(self):
        for line in self.proc.stdout:
            self.lines.put(line)
        self.lines.put(None)  # EOF

    def send(self, msg):
        self.proc.stdin.write((json.dumps(msg) + "\n").encode())
        self.proc.stdin.flush()

    def notify(self, method):
        self.send({"jsonrpc": "2.0", "method": method})

    def request(self, method, params):
        self.next_id += 1
        want = self.next_id
        self.send({"jsonrpc": "2.0", "id": want, "method": method, "params": params})
        deadline = time.time() + CALL_TIMEOUT
        while time.time() < deadline:
            try:
                line = self.lines.get(timeout=max(0, deadline - time.time()))
            except queue.Empty:
                break
            if line is None:
                raise RuntimeError("agentpc mcp exited")
            msg = json.loads(line)
            if msg.get("id") == want:  # skip progress and other notifications
                if "error" in msg:
                    raise RuntimeError(f"{method}: {msg['error']}")
                return msg["result"]
        raise TimeoutError(f"{method} took over {CALL_TIMEOUT}s")

    def tool(self, tool_name, /, **args):
        """Call an agentpc tool; returns (ok, text)."""
        self.calls += 1
        res = self.request("tools/call", {"name": tool_name, "arguments": args})
        text = "\n".join(
            c.get("text", "") for c in res.get("content", []) if c.get("type") == "text"
        )
        return not res.get("isError", False), text

    def desktop(self, vm, tool, /, **args):
        """Call a desktop tool; returns (ok, parsed JSON or raw text)."""
        ok, text = self.tool("use_desktop_tool", name=vm, tool=tool, arguments=args)
        # The JSON reply is the text block that parses (screenshots come first as images).
        for block in reversed(text.split("\n{")):
            candidate = block if block.startswith("{") else "{" + block
            try:
                return ok, json.loads(candidate)
            except ValueError:
                continue
        return ok, text

    def close(self):
        self.proc.stdin.close()
        try:
            self.proc.wait(timeout=60)
        except subprocess.TimeoutExpired:
            self.proc.kill()


class Report:
    def __init__(self):
        self.failed = 0

    def check(self, label, ok, detail=""):
        print(f"  {'PASS' if ok else 'FAIL'}  {label}" + (f": {detail}" if detail and not ok else ""))
        if not ok:
            self.failed += 1
        return ok


def stdout(out):
    """The stdout section of a run_command reply ("exit code: N", then "--- stdout ---"...)."""
    m = re.search(r"^--- stdout ---\n(.*?)(?=^--- stderr ---$|\Z)", out, re.M | re.S)
    return m.group(1).strip() if m else ""


def poll(fn, seconds=30, every=2):
    """Call fn until it returns something truthy or time runs out; returns the last value."""
    end = time.time() + seconds
    while True:
        value = fn()
        if value or time.time() >= end:
            return value
        time.sleep(every)


def gateway_checks(m, vm, r):
    ok, listing = m.tool("list_desktop_tools", name=vm)
    r.check(
        "list_desktop_tools shows required arguments and read-only tools",
        ok and "get_window_state(pid" in listing and "[read-only]" in listing,
        listing[:200],
    )
    ok, err = m.tool("use_desktop_tool", name=vm, tool="type_text", arguments={})
    r.check(
        "a call with missing arguments returns the argument list",
        not ok and "arguments (* = required)" in err and "text*" in err,
        err[:300],
    )
    ok, err = m.tool("use_desktop_tool", name=vm, tool="typetext", arguments={})
    r.check(
        "an unknown tool name is refused with a suggestion",
        not ok and "no desktop tool named" in err and "type_text" in err,
        err[:300],
    )


def ubuntu(m, vm, r):
    linux(m, vm, r, "google-chrome", "Chrome")
    deb_double_click(m, vm, r, "all", "")
    lifecycle(m, vm, r, "ubuntu")


def deb_double_click(m, vm, r, arch, machine):
    """Opening a .deb from the desktop (a double-click, i.e. xdg-open) installs it with apt in a
    terminal window. With `machine`, the package's preinst refuses any other `uname -m`."""
    pkg = f"smoke-click-{arch}"
    gate = f'[ \\"\\$(uname -m)\\" = {machine} ] || exit 1\\n' if machine else ""
    def sh(command, timeout=120):
        return m.tool("run_command", name=vm, command=command, timeout=timeout)
    ok, out = sh(
        f"cd /tmp && rm -rf {pkg} && mkdir -p {pkg}/DEBIAN && printf 'Package: {pkg}\\nVersion: 1\\n"
        f"Architecture: {arch}\\nMaintainer: s <s@s>\\nDescription: s\\n' > {pkg}/DEBIAN/control"
        f" && printf \"#!/bin/sh\\n{gate}\" > {pkg}/DEBIAN/preinst && chmod 755 {pkg}/DEBIAN/preinst"
        f" && mkdir -p ~/Downloads && dpkg-deb --build {pkg} ~/Downloads/{pkg}.deb >/dev/null"
        f" && cd ~/Downloads && (DISPLAY=:0 setsid xdg-open {pkg}.deb >/dev/null 2>&1 &)"
        " && xdg-mime query default application/vnd.debian.binary-package"
    )
    r.check("a .deb opens with agentpc's installer", ok and "agentpc-install-deb.desktop" in out, out[-300:])
    # Done once the package is installed and the installer's apt has exited (dpkg marks it
    # installed a moment before apt lets go of the lock).
    status = poll(lambda: "ok installed" in stdout(sh(
        f"pgrep -x apt-get >/dev/null || dpkg-query -W -f='${{Status}}' {pkg} 2>/dev/null; true")[1]), 120, 3)
    label = f"double-clicking an {arch} .deb whose preinst requires {machine} installs it" if machine \
        else "double-clicking a .deb installs it"
    r.check(label, status, "")
    sh(f"sudo apt-get -o DPkg::Lock::Timeout=60 purge -y -q {pkg} >/dev/null 2>&1; rm -rf ~/Downloads/{pkg}.deb /tmp/{pkg}")


def arch(m, vm, r):
    """Arch Linux ARM has no Google Chrome build; its browser is Chromium."""
    linux(m, vm, r, "chromium", "Chromium")
    lifecycle(m, vm, r, "arch")


def linux(m, vm, r, browser, label):
    ok, out = m.tool("run_command", name=vm, command="uname -sm")
    r.check("run_command runs bash", ok and "Linux" in out, out[:200])
    # Every VM's SSH port is reachable from other VMs and local users: key logins only.
    ok, out = m.tool("run_command", name=vm, command="sudo sshd -T | grep -iE '^(passwordauthentication|kbdinteractiveauthentication) '")
    r.check("sshd takes keys only (no password logins)", ok and stdout(out).lower().split() == ["passwordauthentication", "no", "kbdinteractiveauthentication", "no"], out[-200:])
    gateway_checks(m, vm, r)

    # Reading a page: the browser must open straight to it (no first-run or Terms of
    # Service dialog) and expose its content to the accessibility tree.
    ok, app = m.desktop(
        vm, "launch_app", name=browser, additional_arguments=["https://example.com"]
    )
    pid = app.get("pid") if isinstance(app, dict) else None
    r.check(f"launch_app starts {label}", ok and pid, str(app)[:200])
    if not pid:
        return

    def page_window():
        _, w = m.desktop(vm, "list_windows", pid=pid)
        wins = w.get("windows", []) if isinstance(w, dict) else []
        return next((x for x in wins if "Example Domain" in x.get("title", "")), None)

    win = poll(page_window)
    _, all_windows = m.desktop(vm, "list_windows")
    titles = [w.get("title", "") for w in all_windows.get("windows", [])] if isinstance(all_windows, dict) else []
    r.check(
        f"no {label} first-run dialog",
        not any("Terms of Service" in t for t in titles),
        str(titles),
    )
    r.check(f"{label} shows the page", win is not None, str(titles))
    if win:
        def page_text():
            _, s = m.desktop(
                vm, "get_window_state", pid=pid, window_id=win["window_id"],
                include_screenshot=False, query="documentation", timeout_ms=5000,
            )
            return isinstance(s, dict) and any(
                e.get("in_web_content") for e in s.get("elements", [])
            )
        r.check("get_window_state reads the page's text", poll(page_text, 20))

    # Driving a page with the browser tools, in the driver's own isolated browser.
    ok, prep = m.desktop(
        vm, "browser_prepare", allow_launch=True, profile={"mode": "isolated_new"}, session="smoke"
    )
    bpid = prep.get("prepared_pid") if isinstance(prep, dict) else None
    r.check("browser_prepare launches an isolated browser", ok and bpid, str(prep)[:200])
    if not bpid:
        return
    _, w = m.desktop(vm, "list_windows", pid=bpid)
    wins = w.get("windows", []) if isinstance(w, dict) else []
    if not r.check("the isolated browser has a window", bool(wins), str(w)[:200]):
        return
    ok, state = m.desktop(
        vm, "get_browser_state", pid=bpid, window_id=wins[0]["window_id"], session="smoke"
    )
    tabs = state.get("tabs") if isinstance(state, dict) else None
    if not r.check("get_browser_state binds the tab", ok and tabs, str(state)[:200]):
        return
    ok, nav = m.desktop(
        vm, "browser_navigate", target_id=state["target_id"], tab_id=tabs[0]["tab_id"],
        url="https://example.com", session="smoke",
    )
    r.check("browser_navigate loads a page", ok and isinstance(nav, dict) and nav.get("status") == "ok", str(nav)[:200])

    # A dropped driver connection takes its sessions with it; the next call must say so.
    subprocess.run(
        [m.binary, "ssh", vm, "pkill -f 'cua-driver mcp'"],
        stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
    )
    ok, text = m.tool("use_desktop_tool", name=vm, tool="list_windows", arguments={})
    r.check(
        "after a reconnect, the next desktop call says sessions are gone",
        ok and "reconnected to the desktop driver" in text,
        text[:200],
    )


def lifecycle(m, vm, r, guest):
    """Jobs, files, forwards, checkpoints and reset on `vm`, then offline mode and a
    non-default size in two more VMs (each deleted as soon as it's checked). Guest paths
    are under the home directory: /tmp on Arch is a tmpfs, cleared at every boot."""
    def sh(command, name=vm, **kw):
        return m.tool("run_command", name=name, command=command, **kw)

    ok, out = sh("sleep 2; echo smoke-job-done", background=True)
    job = re.search(r"\(id (\d+)", out)
    if r.check("background run_command returns a job id", ok and job, out[:300]):
        def job_done():
            _, st = m.tool("get_job_status", name=vm, id=int(job.group(1)), tail_lines=5)
            return st if "STATE: exited 0" in st and "smoke-job-done" in st else None
        r.check("get_job_status reports the exit code and log tail", poll(job_done, 30))

    # A job killed before it could record an exit code must not read "running" forever.
    ok, out = sh("sleep 300", background=True)
    job = re.search(r"\(id (\d+)", out)
    kill = re.search(r"kill (\d+)", out)
    if job and kill:
        sh(f"kill {kill.group(1)}")
        def ended():
            _, st = m.tool("get_job_status", name=vm, id=int(job.group(1)), tail_lines=1)
            return st if "ended without an exit code" in st else None
        r.check("a killed background job is reported ended", poll(ended, 20), out[:200])

    ok, out = sh("sleep 60; echo late", timeout=3)
    r.check(
        "a foreground run is killed at its timeout",
        not ok and "timed out after 3s" in out and "late" not in out,
        out[:300],
    )

    tmp = tempfile.mkdtemp(prefix="agentpc-smoke-")
    try:
        up = os.path.join(tmp, "up.txt")
        with open(up, "w") as f:
            f.write("hello from the mac\n" * 1000)
        with open(up, "rb") as f:
            digest = hashlib.sha256(f.read()).hexdigest()
        ok, out = m.tool("upload_file", name=vm, host_path=up, guest_path="smoke-up.txt")
        r.check("upload_file", ok, out[:200])
        ok, out = sh("sha256sum ~/smoke-up.txt")
        r.check("the uploaded file's content matches", ok and digest in out, out[:200])

        sh("mkdir -p ~/smoke-dir/sub && echo x > ~/smoke-dir/sub/f && echo y > ~/smoke-dir/g")
        down = os.path.join(tmp, "down")
        ok, out = m.tool("download_file", name=vm, guest_path="smoke-dir", host_path=down)
        got = [os.path.join(down, "sub", "f"), os.path.join(down, "smoke-dir", "sub", "f")]
        r.check("download_file copies a folder", ok and any(map(os.path.isfile, got)), out[:300])
    finally:
        shutil.rmtree(tmp, ignore_errors=True)

    # A server on the guest's own 127.0.0.1, reached from the Mac through forward_port.
    # python3 where the image has it, else perl (both images have perl).
    sh(
        "mkdir -p ~/smoke-www && echo smoke-served > ~/smoke-www/index.html && cd ~/smoke-www && "
        "if command -v python3 >/dev/null; then exec python3 -m http.server 8765 --bind 127.0.0.1; fi; "
        "exec perl -MIO::Socket::INET -e '$s = IO::Socket::INET->new(LocalAddr => \"127.0.0.1\", "
        "LocalPort => 8765, Listen => 5, ReuseAddr => 1) or die; while ($c = $s->accept) "
        "{ while (<$c>) { last if /^\\r?$/ } print $c \"HTTP/1.0 200 OK\\r\\nContent-Length: 13"
        "\\r\\n\\r\\nsmoke-served\\n\"; close $c }'",
        background=True,
    )
    ok, out = m.tool("forward_port", name=vm, guest_port=8765)
    fwd = re.search(r"127\.0\.0\.1:(\d+)", out) if ok else None
    if r.check("forward_port picks a host port", fwd, out[:200]):
        port = int(fwd.group(1))

        def fetch():
            try:
                with urllib.request.urlopen(f"http://127.0.0.1:{port}/", timeout=5) as resp:
                    return resp.read().decode()
            except Exception:
                return ""
        r.check("the forward reaches a server on the guest's 127.0.0.1", "smoke-served" in poll(fetch, 20))
        ok, out = m.tool("list_forwards", name=vm)
        r.check("list_forwards lists it", ok and f"127.0.0.1:{port}" in out, out[:200])
        ok, out = m.tool("delete_forward", name=vm, host_port=port)
        _, listing = m.tool("list_forwards", name=vm)
        r.check("delete_forward removes it", ok and f"127.0.0.1:{port}" not in listing, f"{out[:150]} | {listing[:150]}")

    # UDP: a QEMU host forward, which reaches a server on the guest's 0.0.0.0 (perl on both images).
    sh(
        "exec perl -MIO::Socket::INET -e '$s = IO::Socket::INET->new(LocalAddr => \"0.0.0.0\", "
        "LocalPort => 8766, Proto => \"udp\") or die; while ($s->recv($d, 2048)) "
        "{ $s->send(\"echo:$d\") }'",
        background=True,
    )
    ok, out = m.tool("forward_port", name=vm, guest_port=8766, protocol="udp")
    fwd = re.search(r"udp 127\.0\.0\.1:(\d+)", out) if ok else None
    if r.check("forward_port with protocol udp", fwd, out[:200]):
        port = int(fwd.group(1))

        def echo():
            s = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
            s.settimeout(3)
            try:
                s.sendto(b"smoke-udp", ("127.0.0.1", port))
                return s.recvfrom(64)[0] == b"echo:smoke-udp"
            except OSError:
                return False
            finally:
                s.close()
        r.check("the UDP forward reaches a server on the guest's 0.0.0.0 and back", poll(echo, 20))
        ok, out = m.tool("list_forwards", name=vm)
        r.check("list_forwards lists the udp forward", ok and f"udp 127.0.0.1:{port}" in out, out[:200])
        ok, out = m.tool("delete_forward", name=vm, host_port=port, protocol="udp")
        _, listing = m.tool("list_forwards", name=vm)
        r.check("delete_forward removes the udp forward", ok and f"127.0.0.1:{port}" not in listing, f"{out[:150]} | {listing[:150]}")

    sh("echo before > ~/smoke-marker")
    ok, out = m.tool("checkpoint_vm", name=vm, label="smoke")
    if r.check("checkpoint_vm", ok, out[:200]):
        sh("echo after > ~/smoke-marker")
        ok, out = m.tool("restore_vm", name=vm, label="smoke")
        _, marker = sh("cat ~/smoke-marker")
        r.check("restore_vm returns to the checkpoint", ok and "before" in marker and "after" not in marker, f"{out[:150]} | {marker[:100]}")
        ok, out = m.tool("delete_checkpoint", name=vm, label="smoke")
        r.check("delete_checkpoint", ok, out[:200])

    ok, out = m.tool("reset_vm", name=vm)
    _, left = sh("ls -d ~/smoke-* 2>/dev/null; echo listed")
    r.check("reset_vm returns to a clean install", ok and left.split()[-1:] == ["listed"] and "smoke-" not in left, f"{out[:150]} | {left[:150]}")

    # Extra VMs one at a time, so at most two run at once.
    extra = f"{vm}-offline"
    try:
        ok, out = m.tool("create_vm", os=guest, name=extra, offline=True)
        if r.check("create_vm offline", ok, out[:200]):
            ok, out = sh("curl -sS -m 8 -o /dev/null https://example.com && echo reached || echo blocked", name=extra, timeout=30)
            r.check("an offline VM has no internet", ok and "blocked" in out and "reached" not in out, out[:200])
    finally:
        m.tool("delete_vm", name=extra)

    extra = f"{vm}-sized"
    try:
        ok, out = m.tool("create_vm", os=guest, name=extra, memory_gb=6, cpus=2)
        if r.check("create_vm memory_gb=6 cpus=2 (cold boot)", ok, out[:200]):
            ok, out = sh("nproc; awk '/MemTotal/ {print int($2 / 1048576 + 0.5)}' /proc/meminfo", name=extra)
            nums = [w for w in out.split() if w.isdigit()]
            r.check("the guest sees 2 CPUs and ~6 GB", ok and nums[-2:] == ["2", "6"], out[:200])
    finally:
        m.tool("delete_vm", name=extra)


def x86apps(m, vm, r, guest="ubuntu"):
    """ubuntu-x86apps / arch-x86apps: downloaded x86_64 programs run through FEX, GUI ones
    included. `guest` is the OS, "ubuntu" or "arch"."""
    def sh(command, timeout=300):
        return m.tool("run_command", name=vm, command=command, timeout=timeout)

    def install(pkg):
        if guest == "arch":
            return f"sudo pacman -Syu --noconfirm --needed {pkg} >/dev/null 2>&1"
        return f"sudo DEBIAN_FRONTEND=noninteractive apt-get install -y -q {pkg} >/dev/null 2>&1"

    ok, out = sh(
        "cd /tmp && curl -fsSL https://nodejs.org/dist/v22.20.0/node-v22.20.0-linux-x64.tar.xz"
        " | tar xJ && node-v22.20.0-linux-x64/bin/node -p process.arch"
    )
    r.check("an x86_64 Node runs (a JIT under translation)", ok and out.strip().endswith("x64"), out[-300:])

    # Go's signal-based preemption crashes under FEX unless GODEBUG=asyncpreemptoff=1,
    # which the image's FEX config sets for x86 programs.
    ok, out = sh(
        "cd /tmp && curl -fsSL https://github.com/cli/cli/releases/download/v2.60.1/gh_2.60.1_linux_amd64.tar.gz"
        " | tar xz && for i in 1 2 3; do gh_2.60.1_linux_amd64/bin/gh --version >/dev/null || exit 1; done && echo ok"
    )
    r.check("an x86_64 Go program runs (3/3)", ok and out.strip().endswith("ok"), out[-300:])

    ok, out = sh("sudo /tmp/node-v22.20.0-linux-x64/bin/node -p process.arch")
    r.check("x86 programs run as root too", ok and out.strip().endswith("x64"), out[-300:])

    # An x86-only installer: a bash script (pipefail, arrays, [[ ]]) that refuses non-x86_64 runs
    # unmodified under FEXBash, which must use bash (an Ubuntu RootFS's /bin/sh is dash).
    script = (
        "#!/usr/bin/env bash\nset -euo pipefail\narch=($(uname -m))\n"
        "[[ ${arch[0]} == x86_64 ]] || { echo refused; exit 10; }\necho \"installer-ok $BASH_VERSION\"\n"
    )
    ok, out = sh(
        f"printf '{script}' > ~/smoke-install.sh && chmod +x ~/smoke-install.sh"
        " && sudo FEXBash ~/smoke-install.sh"
    )
    r.check("an x86-only bash installer runs unmodified under FEXBash", ok and "installer-ok" in stdout(out), out[-300:])
    # The paste-block shape, `curl ... | sudo VAR=... bash -s -- args`: the real sudo is setuid and
    # runs natively, so FEXBash's own sudo keeps the command x86 (and the variable and argument).
    ok, out = sh(
        "sudo FEXBash -c 'cat /home/agent/smoke-install.sh | sudo SMOKE_VAR=7 bash -s -- arg1"
        " && sudo SMOKE_VAR=7 bash -c \"echo var=\\$SMOKE_VAR user=\\$(id -un)\"'; rm -f ~/smoke-install.sh"
    )
    r.check(
        "curl | sudo bash -s inside FEXBash stays x86 (FEXBash's sudo)",
        ok and "installer-ok" in stdout(out) and "var=7 user=root" in stdout(out),
        out[-300:],
    )
    # sudo -E keeps the caller's $HOME: root's FEX must still start (not share the agent's
    # FEXServer), and bundled options (-Eu root) must still reach sudo whole.
    ok, out = sh("FEXBash -c 'sudo -E sh -c \"echo \\$(id -un) \\$(uname -m)\"; sudo -Eu root sh -c \"echo \\$(id -un) \\$(uname -m)\"'")
    r.check(
        "sudo -E and sudo -Eu root inside FEXBash run as root, x86",
        ok and stdout(out).splitlines()[-2:] == ["root x86_64", "root x86_64"],
        out[-300:],
    )

    # An x86 service: a hardened systemd unit (a relay's options) running as nobody, whose home
    # doesn't exist. FEX falls back to a writable directory and reads its RootFS without FUSE or
    # openat2; the agentpc-fex generator relaxes MemoryDenyWriteExecute=/LockPersonality= (which
    # stop any JIT) on its own, at the daemon-reload, with no fex-unit.
    unit = "\n".join([
        "[Service]", "Type=oneshot", "User=nobody",
        "ExecStart=/usr/local/lib/smoke-node/bin/node -p process.arch",
        "NoNewPrivileges=yes", "ProtectSystem=strict", "ProtectHome=yes", "PrivateTmp=yes",
        "PrivateDevices=yes", "RestrictSUIDSGID=yes", "RestrictNamespaces=yes",
        "MemoryDenyWriteExecute=yes", "LockPersonality=yes",
    ])
    ok, out = sh(
        "sudo rm -rf /usr/local/lib/smoke-node && sudo cp -r /tmp/node-v22.20.0-linux-x64 /usr/local/lib/smoke-node"
        f" && printf '%s\\n' '{unit}' | sudo tee /etc/systemd/system/smoke-x86.service >/dev/null"
        " && sudo systemctl daemon-reload && sudo systemctl start smoke-x86"
        " && sudo journalctl -u smoke-x86 -n 5 --no-pager -o cat"
    )
    r.check("a hardened systemd unit runs an x86 program (generator, no fex-unit)", ok and "x64" in stdout(out).split(), out[-300:])
    ok, out = sh("fex-unit --help")
    r.check("fex-unit --help prints its usage", ok and "usage: fex-unit" in out, out[-200:])
    sh("sudo rm -rf /etc/systemd/system/smoke-x86.service /usr/local/lib/smoke-node; sudo systemctl daemon-reload")

    # x86 libraries the RootFS lacks: on Ubuntu from apt (multiarch), on Arch (no multiarch)
    # installed into the x86 root filesystem with fex-pacman. Either way FEX finds them.
    load = " && FEXBash -c 'python3 -c \"import ctypes; ctypes.CDLL(\\\"libzmq.so.5\\\"); print(\\\"loaded\\\")\"'"
    if guest == "ubuntu":
        deb_double_click(m, vm, r, "amd64", "x86_64")
        ok, out = sh(install("libzmq5:amd64") + load)
        r.check("apt install <lib>:amd64 gives x86 programs the library", ok and "loaded" in out.split(), out[-300:])
        # An amd64 .deb whose maintainer scripts refuse anything but x86_64: dpkg runs them
        # natively, so uname/arch/dpkg answer as x86 while DPKG_MAINTSCRIPT_ARCH is amd64. An
        # arm64 package's scripts still see the real machine.
        check = (
            "#!/bin/sh\\n[ \\\"\\$(uname -m)/\\$(arch)/\\$(dpkg --print-architecture)\\\" = \\\"%s\\\" ]"
            " || { echo \\\"refused \\$(uname -m)\\\" >&2; exit 1; }\\n"
        )
        ok, out = sh(
            "cd /tmp && rm -rf smoke-deb && for a in amd64:x86_64/x86_64/amd64 arm64:aarch64/aarch64/arm64; do"
            " d=smoke-deb/${a%%:*}; mkdir -p $d/DEBIAN"
            " && printf 'Package: smoke-%s\\nVersion: 1\\nArchitecture: %s\\nMaintainer: s <s@s>\\nDescription: s\\n'"
            " ${a%%:*} ${a%%:*} > $d/DEBIAN/control"
            f" && printf \"{check}\" ${{a#*:}} > $d/DEBIAN/preinst && cp $d/DEBIAN/preinst $d/DEBIAN/postinst"
            " && chmod 755 $d/DEBIAN/preinst $d/DEBIAN/postinst && dpkg-deb --build $d smoke-deb/${a%%:*}.deb >/dev/null"
            " || exit 1; done"
            " && sudo apt-get install -y -q ./smoke-deb/amd64.deb ./smoke-deb/arm64.deb >/dev/null"
            " && dpkg-query -W -f='${Package}:${Architecture}=${db:Status-Abbrev}\\n' smoke-amd64 smoke-arm64"
            "; sudo dpkg --purge smoke-amd64 smoke-arm64 >/dev/null 2>&1; rm -rf smoke-deb"
        )
        r.check(
            "an amd64 .deb's maintainer scripts see x86_64; arm64 ones see aarch64",
            ok and "smoke-amd64:amd64=ii" in out and "smoke-arm64:arm64=ii" in out,
            out[-300:],
        )
    else:
        ok, out = sh("sudo fex-pacman -Sy --noconfirm --needed zeromq >/dev/null 2>&1" + load)
        r.check("fex-pacman gives x86 programs the library", ok and "loaded" in out.split(), out[-300:])
        # fex-pacman leaves nothing behind: no mounts under the tree, no chroot marker, and
        # FEX still sees the guest's own user.
        rootfs = "/usr/share/fex-emu/RootFS/ArchLinux"
        ok, out = sh(f"findmnt -rn -o TARGET -R {rootfs} | grep -vx {rootfs}; true")
        r.check("fex-pacman unmounts its chroot", ok and not stdout(out), out[-300:])
        ok, out = sh(f"test -e {rootfs}/run/.containerenv && echo present || echo absent")
        r.check("fex-pacman removes the .containerenv marker", ok and stdout(out) == "absent", out[-300:])
        ok, out = sh("FEXBash -c 'id -un'")
        r.check("FEXBash runs as agent after fex-pacman", ok and stdout(out).splitlines()[-1:] == ["agent"], out[-300:])

    # The FEX build in the image is the one guests/<os>/x86apps.sh pins.
    script = os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "guests", guest, "x86apps.sh")
    with open(script) as f:
        pinned = re.search(r"^fex_version=(\S+)", f.read(), re.M).group(1)
    ok, out = sh("cat /var/lib/agentpc/fex-version")
    r.check(f"FEX {pinned} (the pinned version) is installed", ok and stdout(out) == pinned, out[-300:])

    # agentpc turns on the CPU's TSO mode for x86apps VMs (macOS 15+) and then tells FEX it
    # needn't emulate x86 memory ordering; on older macOS FEX emulates it.
    mac = platform.mac_ver()[0]
    hw = bool(mac) and int(mac.split(".")[0]) >= 15
    tso, emulation = ("hardware", "Disabled") if hw else ("emulated", "Enabled")
    ok, out = sh("FEXGetConfig --tso-emulation-info | grep 'TSO Emulation:'")
    r.check(f"{tso} TSO: FEX emulation {emulation.lower()}", ok and emulation in out, out[-300:])
    ok, listing = m.tool("list_vms")
    vm_entry = next(
        (i for i in json.loads(listing).get("instances", []) if i.get("name") == vm), {}
    ) if ok else {}
    r.check(f"list_vms reports x86_tso {tso}", vm_entry.get("x86_tso") == tso, str(vm_entry)[:300])

    # x86 containers run through the image's static FEX.
    if guest == "arch":
        ok, out = sh(install("docker"), timeout=600)
        r.check("pacman installs docker", ok, out[-300:])
        # -Syu may have upgraded the kernel, whose modules docker needs: reboot into it.
        ok, out = sh("test -d /usr/lib/modules/$(uname -r) && echo present || echo missing")
        if ok and stdout(out) == "missing":
            ok, out = m.tool("stop_vm", name=vm)
            if ok:
                ok, out = m.tool("start_vm", name=vm)
            r.check("reboot into the upgraded kernel", ok, out[-300:])
        docker = "sudo systemctl start docker"
    else:
        docker = install("docker.io")
    ok, out = sh(
        docker
        + " && sudo docker run --rm --platform linux/amd64 alpine uname -m 2>/dev/null",
        timeout=600,
    )
    r.check("an amd64 container runs", ok and "x86_64" in out.split(), out[-300:])

    ok, out = sh("du -sk ~/.cache/fex-emu | cut -f1")
    kb = stdout(out).splitlines()[-1] if stdout(out) else ""
    r.check("FEX caches translated code on disk", ok and kb.isdigit() and int(kb) > 0, out[-200:])

    ok, out = sh(
        "cd /tmp && curl -fsSL 'https://download.mozilla.org/?product=firefox-latest&os=linux64&lang=en-US'"
        " | tar xJ && file -L firefox/firefox-bin | grep -o x86-64",
        timeout=600,
    )
    if guest == "arch":
        # /tmp is a tmpfs on Arch: the x86 downloads above must leave room in it.
        _, df = sh("df -Pk /tmp | awk 'NR == 2 {print $4}'")
        free = df.split()[-1] if df.split() else ""
        r.check("/tmp keeps 512 MB free after the x86 downloads", free.isdigit() and int(free) >= 512 * 1024, df[-200:])
    if not r.check("the x86_64 Firefox downloads", ok and "x86-64" in out, out[-300:]):
        return
    ok, app = m.desktop(
        vm, "launch_app", name="/tmp/firefox/firefox", additional_arguments=["https://example.com"]
    )
    pid = app.get("pid") if isinstance(app, dict) else None
    if not r.check("launch_app starts the x86_64 Firefox", ok and pid, str(app)[:200]):
        return

    def page_window():
        _, w = m.desktop(vm, "list_windows")
        wins = w.get("windows", []) if isinstance(w, dict) else []
        return next((x for x in wins if "Example Domain" in x.get("title", "")), None)

    r.check("the x86_64 Firefox shows the page", poll(page_window, 120, 5) is not None)


# A pinned x64 build (a versioned file, so it can't change under the check).
BUSYBOX_URL = "https://frippery.org/files/busybox/busybox-w64-FRP-6075-g169694ebd.exe"
BUSYBOX_SHA256 = "07BB1E5B095B00D68A695481F9240879F33C5724B40AA2308F999D54ED78F075"


def windows_lifecycle(m, vm, r):
    """Jobs (a scheduled task, not Linux's setsid), forwards, checkpoints and reset on
    Windows, whose paths through ops differ from the Linux ones."""
    def ps(command, **kw):
        return m.tool("run_command", name=vm, command=command, **kw)

    ok, out = ps("& \"$env:windir\\System32\\OpenSSH\\sshd.exe\" -T | Select-String -Pattern '^(passwordauthentication|kbdinteractiveauthentication) '")
    r.check("windows: sshd takes keys only (no password logins)", ok and "passwordauthentication no" in out and "kbdinteractiveauthentication no" in out, out[-200:])

    ok, out = ps("Start-Sleep 2; 'smoke-job-done'", background=True)
    job = re.search(r"\(id (\d+)", out)
    if r.check("windows: background run_command returns a job id", ok and job, out[:300]):
        def job_done():
            _, st = m.tool("get_job_status", name=vm, id=int(job.group(1)), tail_lines=5)
            return st if "STATE: exited 0" in st and "smoke-job-done" in st else None
        r.check("windows: get_job_status reports the exit code and log tail", poll(job_done, 60))

    # A cmdlet failure sets no exit code; the job must still not report success.
    ok, out = ps("Get-Item C:\\no-such-file", background=True)
    job = re.search(r"\(id (\d+)", out)
    if job:
        def failed():
            _, st = m.tool("get_job_status", name=vm, id=int(job.group(1)), tail_lines=5)
            return st if "STATE: exited" in st else None
        st = poll(failed, 60) or ""
        r.check("windows: a background job whose cmdlet fails exits non-zero", "STATE: exited 1" in st, st[:200])

    ok, out = ps("Start-Sleep 300", background=True)
    job = re.search(r"\(id (\d+)", out)
    if job:
        time.sleep(3)
        ps(f"Stop-ScheduledTask agentpc-bg-{job.group(1)}")
        def ended():
            _, st = m.tool("get_job_status", name=vm, id=int(job.group(1)), tail_lines=1)
            return st if "ended without an exit code" in st else None
        r.check("windows: a stopped background job is reported ended", poll(ended, 30), out[:200])

    ok, out = ps("Start-Sleep 60; 'late'", timeout=3)
    r.check("windows: a foreground run stops at its timeout", not ok and "timed out after 3s" in out and "late" not in out, out[:300])

    # TCP: the guest's own OpenSSH server, through an SSH tunnel.
    ok, out = m.tool("forward_port", name=vm, guest_port=22)
    fwd = re.search(r"127\.0\.0\.1:(\d+)", out) if ok else None
    if r.check("windows: forward_port", fwd, out[:200]):
        port = int(fwd.group(1))

        def banner():
            try:
                with socket.create_connection(("127.0.0.1", port), timeout=5) as c:
                    return c.recv(64).startswith(b"SSH-")
            except OSError:
                return False
        r.check("windows: the forward reaches the guest's sshd", poll(banner, 20))
        m.tool("delete_forward", name=vm, host_port=port)

    # UDP: a QEMU host forward to a server on 0.0.0.0, which Windows' firewall must allow.
    ps("New-NetFirewallRule -DisplayName smoke-udp -Direction Inbound -Protocol UDP -LocalPort 8766 -Action Allow | Out-Null")
    ps(
        "$u = New-Object System.Net.Sockets.UdpClient 8766; while ($true) { "
        "$ep = New-Object System.Net.IPEndPoint([System.Net.IPAddress]::Any, 0); $d = $u.Receive([ref]$ep); "
        "$b = [Text.Encoding]::ASCII.GetBytes('echo:' + [Text.Encoding]::ASCII.GetString($d)); "
        "[void]$u.Send($b, $b.Length, $ep) }",
        background=True,
    )
    ok, out = m.tool("forward_port", name=vm, guest_port=8766, protocol="udp")
    fwd = re.search(r"udp 127\.0\.0\.1:(\d+)", out) if ok else None
    if r.check("windows: forward_port with protocol udp", fwd, out[:200]):
        port = int(fwd.group(1))

        def echo():
            u = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
            u.settimeout(3)
            try:
                u.sendto(b"smoke-udp", ("127.0.0.1", port))
                return u.recvfrom(64)[0] == b"echo:smoke-udp"
            except OSError:
                return False
            finally:
                u.close()
        r.check("windows: the UDP forward reaches the guest and back", poll(echo, 30))
        m.tool("delete_forward", name=vm, host_port=port, protocol="udp")

    ps("Set-Content $HOME\\smoke-marker before")
    ok, out = m.tool("checkpoint_vm", name=vm, label="smoke")
    if r.check("windows: checkpoint_vm", ok, out[:200]):
        ps("Set-Content $HOME\\smoke-marker after")
        ok, out = m.tool("restore_vm", name=vm, label="smoke")
        _, marker = ps("Get-Content $HOME\\smoke-marker")
        r.check("windows: restore_vm returns to the checkpoint", ok and "before" in marker and "after" not in marker, f"{out[:150]} | {marker[:100]}")
        m.tool("delete_checkpoint", name=vm, label="smoke")

    ok, out = m.tool("reset_vm", name=vm)
    _, left = ps("Test-Path $HOME\\smoke-marker")
    r.check("windows: reset_vm returns to a clean install", ok and "False" in left, f"{out[:150]} | {left[:100]}")


def windows(m, vm, r):
    ok, out = m.tool("run_command", name=vm, command="$PSVersionTable.PSEdition")
    r.check("run_command runs PowerShell", ok and "Desktop" in out, out[:200])
    gateway_checks(m, vm, r)
    windows_lifecycle(m, vm, r)

    # The guest is ARM64; x64 programs run through Prism, and see an AMD64 environment.
    ok, out = m.tool(
        "run_command", name=vm, timeout=180,
        command=(
            "$ProgressPreference = 'SilentlyContinue'; $f = \"$env:TEMP\\busybox.exe\"; "
            f"Invoke-WebRequest {BUSYBOX_URL} -OutFile $f; "
            f"if ((Get-FileHash $f).Hash -ne '{BUSYBOX_SHA256}') {{ 'busybox checksum mismatch'; exit 1 }}; "
            "\"host=$env:PROCESSOR_ARCHITECTURE\"; & $f sh -c 'echo x64=$PROCESSOR_ARCHITECTURE'"
        ),
    )
    r.check(
        "an x64 program runs through Prism",
        ok and "host=ARM64" in out and "x64=AMD64" in out,
        out[:300],
    )

    ok, app = m.desktop(vm, "launch_app", name="notepad")
    pid = app.get("pid") if isinstance(app, dict) else None
    if not r.check("launch_app starts Notepad", ok and pid, str(app)[:200]):
        return
    time.sleep(3)
    _, w = m.desktop(vm, "list_windows", pid=pid)
    wins = w.get("windows", []) if isinstance(w, dict) else []
    titles = [x.get("title", "") for x in wins]
    r.check("no Notepad first-run tip", "PopupHost" not in titles, str(titles))
    main = next((x for x in wins if "Notepad" in x.get("title", "")), None)
    if not r.check("Notepad has a window", main is not None, str(titles)):
        return

    # Type into the page. Elements when the tree is there, pixels when it's "degraded"
    # (modern Notepad's tree is sometimes empty).
    phrase = "agentpc smoke test"
    _, s = m.desktop(vm, "get_window_state", pid=pid, window_id=main["window_id"])
    if not isinstance(s, dict):
        r.check("get_window_state answers", False, str(s)[:200])
        return
    edit = next((e for e in s.get("elements", []) if e.get("role") in ("Document", "Edit")), None)
    if edit:
        args = {"snapshot_id": s["snapshot_id"], "element_index": edit["element_index"]}
    else:
        args = {"x": s.get("screenshot_width", 640) // 2, "y": s.get("screenshot_height", 360) // 2}
    ok, typed = m.desktop(vm, "type_text", pid=pid, window_id=main["window_id"], text=phrase, **args)
    r.check(f"type_text ({'elements' if edit else 'pixels'})", ok, str(typed)[:200])

    def titled():
        _, w = m.desktop(vm, "list_windows", pid=pid)
        return isinstance(w, dict) and any(
            phrase in x.get("title", "") for x in w.get("windows", [])
        )
    # Notepad names an unsaved tab after its first line.
    r.check("the text landed in Notepad", poll(titled, 15))


def reap_stale_vms(binary):
    """Delete smoke VMs a killed earlier run left behind (its MCP server is gone, so their
    owner isn't running); a concurrent run's VMs are left alone."""
    m = Mcp(binary)
    try:
        ok, out = m.tool("list_vms")
        if not ok:
            return
        listing = json.loads(out[out.index("{"):])
        for vm in listing.get("instances", []):
            # Ours: a smoke- name, made through this suite's MCP client, whose server is gone.
            if (
                vm["name"].startswith("smoke-")
                and (vm.get("owner") or "").startswith("agentpc-smoke [")
                and vm.get("owner_running") is False
            ):
                print(f"deleting {vm['name']}, left by an earlier run")
                m.tool("delete_vm", name=vm["name"])
    finally:
        m.close()


def main():
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("--bin", default="agentpc", help="agentpc binary to test")
    ap.add_argument("--os", default="ubuntu,windows", help="comma-separated: ubuntu,windows,x86apps,arch,arch-x86apps")
    a = ap.parse_args()
    names = [o.strip() for o in a.os.split(",") if o.strip()]
    known = {"ubuntu", "windows", "x86apps", "arch", "arch-x86apps"}
    if not names or any(n not in known for n in names):
        ap.error(f"--os takes one or more of: {', '.join(sorted(known))}")

    reap_stale_vms(a.bin)
    failed = 0
    for os_name in names:
        vm = f"smoke-{os_name}-{os.getpid()}"
        print(f"{os_name}: {vm}")
        m = Mcp(a.bin)
        r = Report()
        start = time.time()
        try:
            if os_name == "x86apps":
                ok, out = m.tool("create_vm", os="ubuntu", version="x86apps", name=vm)
            elif os_name == "arch-x86apps":
                ok, out = m.tool("create_vm", os="arch", version="x86apps", name=vm)
            else:
                ok, out = m.tool("create_vm", os=os_name, name=vm)
            if r.check("create_vm", ok, out[:300]):
                {
                    "ubuntu": ubuntu,
                    "windows": windows,
                    "x86apps": x86apps,
                    "arch": arch,
                    "arch-x86apps": lambda m, vm, r: x86apps(m, vm, r, "arch"),
                }[os_name](m, vm, r)
        except Exception as e:  # report and keep going to cleanup
            r.check("no unexpected error", False, repr(e)[:300])
        finally:
            # A VM left behind is a failure too (it would pile up across runs).
            try:
                ok, out = m.tool("delete_vm", name=vm)
                r.check("delete_vm", ok or "no instance" in out, out[:200])
            except Exception as e:
                r.check("delete_vm", False, repr(e)[:200])
            finally:
                m.close()
        print(f"  {os_name}: {r.failed} failed, {m.calls} tool calls, {time.time() - start:.0f}s")
        failed += r.failed
    sys.exit(1 if failed else 0)


if __name__ == "__main__":
    main()
