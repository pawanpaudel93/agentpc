#!/usr/bin/env python3
"""Smoke test: drive fresh VMs through agentpc's MCP server the way an agent does.

Run after changing an image, a guest script or the desktop driver:

    cargo build --release
    scripts/smoke.py --bin target/release/agentpc            # ubuntu and windows
    scripts/smoke.py --bin target/release/agentpc --os ubuntu
    scripts/smoke.py --bin target/release/agentpc --os x86apps  # ubuntu-x86apps (not a default)

Each OS gets its own throwaway VM (deleted afterwards). Checks that the guest isn't blocked
by first-run dialogs, that the desktop tools read and type, and that the gateway explains
bad arguments. Needs internet in the guest (example.com). Exits 1 if any check fails.
Standard library only.
"""

import argparse
import json
import os
import select
import subprocess
import sys
import time

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
        out = self.proc.stdout
        while time.time() < deadline:
            ready, _, _ = select.select([out], [], [], deadline - time.time())
            if not ready:
                break
            line = out.readline()
            if not line:
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
    ok, out = m.tool("run_command", name=vm, command="uname -sm")
    r.check("run_command runs bash", ok and "Linux" in out, out[:200])
    gateway_checks(m, vm, r)

    # Reading a page: Chrome must open straight to it (no Terms of Service dialog) and
    # expose its content to the accessibility tree.
    ok, app = m.desktop(
        vm, "launch_app", name="google-chrome", additional_arguments=["https://example.com"]
    )
    pid = app.get("pid") if isinstance(app, dict) else None
    r.check("launch_app starts Chrome", ok and pid, str(app)[:200])
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
        "no Chrome first-run dialog",
        not any("Terms of Service" in t for t in titles),
        str(titles),
    )
    r.check("Chrome shows the page", win is not None, str(titles))
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


def x86apps(m, vm, r):
    """ubuntu-x86apps: downloaded x86_64 programs run through FEX, GUI ones included."""
    def sh(command, timeout=300):
        return m.tool("run_command", name=vm, command=command, timeout=timeout)

    ok, out = sh(
        "cd /tmp && curl -fsSL https://nodejs.org/dist/v22.20.0/node-v22.20.0-linux-x64.tar.xz"
        " | tar xJ && node-v22.20.0-linux-x64/bin/node -p process.arch"
    )
    r.check("an x86_64 Node runs (a JIT under translation)", ok and out.strip().endswith("x64"), out[-300:])

    # Go's signal-based preemption crashes under FEX unless GODEBUG=asyncpreemptoff=1,
    # which the image's FEX config sets for x86 programs.
    ok, out = sh(
        "cd /tmp && curl -fsSL https://github.com/cli/cli/releases/download/v2.60.1/gh_2.60.1_linux_amd64.tar.gz"
        " | tar xz && for i in 1 2 3; do gh_2.60.1_linux_amd64/bin/gh --version >/dev/null || exit 1; done; echo ok"
    )
    r.check("an x86_64 Go program runs (3/3)", ok and out.strip().endswith("ok"), out[-300:])

    ok, out = sh("sudo /tmp/node-v22.20.0-linux-x64/bin/node -p process.arch")
    r.check("x86 programs run as root too", ok and out.strip().endswith("x64"), out[-300:])

    # Multiarch: x86 libraries the RootFS lacks come from apt, and FEX finds them.
    ok, out = sh(
        "sudo DEBIAN_FRONTEND=noninteractive apt-get install -y -q libzmq5:amd64 >/dev/null 2>&1"
        " && FEXBash -c 'python3 -c \"import ctypes; ctypes.CDLL(\\\"libzmq.so.5\\\"); print(\\\"loaded\\\")\"'"
    )
    r.check("apt install <lib>:amd64 gives x86 programs the library", ok and "loaded" in out.split(), out[-300:])

    ok, out = sh("du -sk ~/.cache/fex-emu | cut -f1")
    kb = out.strip().splitlines()[-1] if out.strip() else ""
    r.check("FEX caches translated code on disk", ok and kb.isdigit() and int(kb) > 0, out[-200:])

    ok, out = sh(
        "cd /tmp && curl -fsSL 'https://download.mozilla.org/?product=firefox-latest&os=linux64&lang=en-US'"
        " | tar xJ && file -L firefox/firefox-bin | grep -o x86-64",
        timeout=600,
    )
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


def windows(m, vm, r):
    ok, out = m.tool("run_command", name=vm, command="$PSVersionTable.PSEdition")
    r.check("run_command runs PowerShell", ok and "Desktop" in out, out[:200])
    gateway_checks(m, vm, r)

    # The guest is ARM64; x64 programs run through Prism, and see an AMD64 environment.
    ok, out = m.tool(
        "run_command", name=vm, timeout=180,
        command=(
            "$ProgressPreference = 'SilentlyContinue'; $f = \"$env:TEMP\\busybox.exe\"; "
            "Invoke-WebRequest https://frippery.org/files/busybox/busybox64.exe -OutFile $f; "
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


def main():
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("--bin", default="agentpc", help="agentpc binary to test")
    ap.add_argument("--os", default="ubuntu,windows", help="comma-separated: ubuntu,windows,x86apps")
    a = ap.parse_args()

    failed = 0
    for os_name in [o.strip() for o in a.os.split(",") if o.strip()]:
        vm = f"smoke-{os_name}-{os.getpid()}"
        print(f"{os_name}: {vm}")
        m = Mcp(a.bin)
        r = Report()
        start = time.time()
        try:
            if os_name == "x86apps":
                ok, out = m.tool("create_vm", os="ubuntu", version="x86apps", name=vm)
            else:
                ok, out = m.tool("create_vm", os=os_name, name=vm)
            if r.check("create_vm", ok, out[:300]):
                {"ubuntu": ubuntu, "windows": windows, "x86apps": x86apps}[os_name](m, vm, r)
        except Exception as e:  # report and keep going to cleanup
            r.check("no unexpected error", False, repr(e)[:300])
        finally:
            try:
                m.tool("delete_vm", name=vm)
            except Exception as e:
                print(f"  (could not delete {vm}: {e})")
            m.close()
        print(f"  {os_name}: {r.failed} failed, {m.calls} tool calls, {time.time() - start:.0f}s")
        failed += r.failed
    sys.exit(1 if failed else 0)


if __name__ == "__main__":
    main()
