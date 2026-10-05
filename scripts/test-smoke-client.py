#!/usr/bin/env python3
"""Regression: smoke.py's concurrent MCP client, its recovery-check helpers, and (optionally)
its VM-free protocol checks.

    scripts/test-smoke-client.py               # everything, incl. the real built server
    scripts/test-smoke-client.py --fake-only   # no agentpc server is started at all

The recovery checks in smoke.py (cancelled requests, dropped clients, job listing, readiness)
need a client that can leave a request in flight, send notifications/cancelled for it, forget
it, and read other replies meanwhile. Those parts run against a fake server (a small script
that logs only message kinds, never contents) and against canned replies; the checks that must
not pass vacuously (processes proven to exist before a stop, a readiness fixture that is
missing) are tested with canned replies too.

The full run also executes smoke.py's protocol_checks against the real built binary over a
ghost VM name. That server runs in a fresh, empty AGENTPC_HOME with AGENTPC_KEEP_RUNNING=1: it
sees no VM at all and never stops any. With --fake-only that part is NOT run, and the final line
says so. The tests print no reply text.
"""

import importlib.util
import os
import stat
import sys
import tempfile
import time
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
FAKE_ONLY = "--fake-only" in sys.argv[1:]
spec = importlib.util.spec_from_file_location("smoke", ROOT / "scripts" / "smoke.py")
smoke = importlib.util.module_from_spec(spec)
spec.loader.exec_module(smoke)

errors = []


def require(condition, message):
    if not condition:
        errors.append(message)


# A fake `agentpc mcp`: JSON lines in and out. `slow` is answered only when `release` arrives
# (so concurrency is proven by ordering, not by timing), `hang` never is, `echo_env` reports
# SMOKE_TEST_VAR, and every received message is logged as "<method> <id> <requestId>".
FAKE = r'''#!/usr/bin/env python3
import json, os, sys
log = open(os.environ["FAKE_LOG"], "a", buffering=1)
def reply(i, result):
    sys.stdout.write(json.dumps({"jsonrpc": "2.0", "id": i, "result": result}) + "\n")
    sys.stdout.flush()
def text(s, **extra):
    return {"content": [{"type": "text", "text": s}], "isError": False, **extra}
held = []
for line in sys.stdin:
    msg = json.loads(line)
    method, i = msg.get("method"), msg.get("id")
    params = msg.get("params") or {}
    log.write(f"{method} {i} {params.get('requestId')}\n")
    if method == "initialize":
        reply(i, {"protocolVersion": "2025-06-18", "capabilities": {}, "serverInfo": {"name": "fake", "version": "1"}})
    elif method == "tools/list":
        reply(i, {"tools": [{"name": "echo"}]})
    elif method == "tools/call":
        name = params["name"]
        if name == "slow":
            held.append(i)
        elif name == "release":
            for h in held:  # answer the held requests first, then this one
                reply(h, text("slow done"))
            held.clear()
            reply(i, text("released"))
        elif name == "hang":
            pass
        elif name == "echo_env":
            reply(i, text(os.environ.get("SMOKE_TEST_VAR", "unset")))
        elif name == "error":
            sys.stdout.write(json.dumps({"jsonrpc": "2.0", "id": i, "error": {"code": -32602, "message": "bad"}}) + "\n")
            sys.stdout.flush()
        else:
            reply(i, text("echo " + name, structuredContent={"state": "exited"}))
log.write("EOF\n")
'''

with tempfile.TemporaryDirectory() as tmp:
    tmp = Path(tmp)
    fake = tmp / "fake-agentpc"
    fake.write_text(FAKE)
    fake.chmod(fake.stat().st_mode | stat.S_IEXEC)
    logfile = tmp / "log"
    os.environ["FAKE_LOG"] = str(logfile)
    os.environ["SMOKE_TEST_VAR"] = "inherited"

    def logged():
        return logfile.read_text().splitlines() if logfile.exists() else []

    m = smoke.Mcp(str(fake))
    try:
        # 1. Serial calls behave as they always did.
        res = m.call("fast")
        require(res["content"][0]["text"] == "echo fast" and smoke.structured(res) == {"state": "exited"},
                "a serial call returns its whole result")
        require(m.calls == 1 and len(m.log) == 1, "the call is still counted and measured")
        try:
            m.call("error")
            require(False, "a JSON-RPC error raises")
        except RuntimeError:
            pass

        # 2. Requests overlap: `slow` cannot be answered before `release` reaches the server, so
        #    getting `fast` and `release` back while `slow` is pending, and `slow` matched to its
        #    own id afterwards, proves concurrent matching without any timing assumption.
        rid, fut = m.start("tools/call", {"name": "slow", "arguments": {}})
        require(m.call("fast")["content"][0]["text"] == "echo fast" and not fut.done(),
                "another call is answered while one is in flight")
        require(m.call("release")["content"][0]["text"] == "released",
                "a later request is answered with its own reply")
        done = m.finish("tools/call", fut, timeout=30)
        require(done["content"][0]["text"] == "slow done", "the held reply reaches its own request")

        # 3. notifications/cancelled carries the request's id, and a forgotten request's reply
        #    (or its absence) never reaches anyone.
        rid, fut = m.start("tools/call", {"name": "hang", "arguments": {}})
        m.cancel(rid)
        m.forget(rid)
        m.call("fast")  # a round trip, so the server has read the cancel
        require(f"notifications/cancelled None {rid}" in logged(), "the cancel names the request id")
        require(not fut.done(), "a forgotten request is never completed")
        rid2, fut2 = m.start("tools/call", {"name": "slow", "arguments": {}})
        m.forget(rid2)
        m.call("release")  # the server now answers the forgotten request
        require(m.call("fast")["content"][0]["text"] == "echo fast", "a late reply to a forgotten request is dropped")
        require(not fut2.done(), "and it completes nothing")

        # 4. The server's end ends every pending request with an error, not a hang.
        rid3, fut3 = m.start("tools/call", {"name": "hang", "arguments": {}})
    finally:
        began = time.monotonic()
        m.close()
        closed_in = time.monotonic() - began
    try:
        m.finish("tools/call", fut3, timeout=30)
        require(False, "a request pending at the server's exit raises")
    except RuntimeError:
        pass
    require(closed_in < 30, "close waits for the server's exit, boundedly")
    require("EOF" in logged(), "close ends the server's input")
    require(m.proc.returncode is not None, "close reaps the child")
    require(m.proc.stdin.closed and m.proc.stdout.closed, "close releases the pipes")

    # 5. Environment: inherited by default, overridden by env=.
    for env, want in ((None, "inherited"), ({"SMOKE_TEST_VAR": "override"}, "override")):
        other = smoke.Mcp(str(fake), env=env)
        try:
            require(other.call("echo_env")["content"][0]["text"] == want, f"env {env} reaches the server as {want}")
        finally:
            other.close()


# 5b. Recovery helpers against canned replies (no server, no VM).
class Recorder:
    """A Report stand-in that records checks instead of printing them."""

    def __init__(self):
        self.checks = []

    def check(self, label, ok, detail=""):
        self.checks.append((label, bool(ok)))
        return bool(ok)

    @property
    def failed(self):
        return sum(1 for _, ok in self.checks if not ok)


class Scripted:
    """Just enough Mcp: call() answers from handler(tool, args); tool() is a no-op success."""

    def __init__(self, handler):
        self.handler, self.seen, self.log, self.calls = handler, [], [], 0

    def call(self, tool, /, **args):
        self.seen.append((tool, args))
        return self.handler(tool, args)

    def tool(self, tool, /, **args):
        self.seen.append((tool, args))
        return True, "{}"

    def measured(self, since):
        return 0, 0, 0.0

    def start_call(self, tool, /, **args):
        from concurrent.futures import Future

        self.seen.append((tool, args))
        fut = Future()
        fut.set_result({"result": self.handler(tool, args)})
        return 1, fut

    def finish(self, method, fut, timeout=None):
        return fut.result()["result"]

    def cancel(self, request_id, reason=""):
        self.seen.append(("cancel", request_id))

    def forget(self, request_id):
        pass


def result(**fields):
    return {"isError": False, "content": [{"type": "text", "text": "x"}], "structuredContent": fields}


def page(jobs, cursor=None, more=False, error=None, code=None):
    body = {"jobs": jobs, "has_more": more, "next_cursor": cursor}
    if error:
        body.update(error=error, error_code=code)
    return {"isError": bool(error), "content": [{"type": "text", "text": "x"}], "structuredContent": body}


walk = Scripted(lambda t, a: [page([{"job_id": 9}, {"job_id": 8}], "c1", True), page([{"job_id": 7}])][len(walk.seen) - 1])
require([j["job_id"] for j in smoke.list_all_jobs(walk, "vm")] == [9, 8, 7], "list_all_jobs follows next_cursor")
require(walk.seen[1][1].get("cursor") == "c1" and walk.seen[0][1].get("limit") == 100,
        "list_all_jobs asks for 100 and passes the cursor on")
require(smoke.list_all_jobs(Scripted(lambda t, a: page([], error="bad", code="invalid_cursor")), "vm") is None,
        "list_all_jobs reports an error page as None")
require(smoke.stale_cursor_before(Scripted(lambda t, a: page([{"job_id": 3}], "c", True)), "vm") == "c",
        "a cursor is taken when one exists")
require(smoke.stale_cursor_before(Scripted(lambda t, a: page([{"job_id": 3}])), "vm") is None,
        "no cursor when the VM has one page")
rec = Recorder()
answers = iter([page([], error="the VM's generation changed, call list_jobs without cursor", code="cursor_stale"),
                page([{"job_id": 1}])])
smoke.stale_cursor_after(Scripted(lambda t, a: next(answers)), "vm", rec, "old")
require(rec.checks and rec.failed == 0, f"a stale cursor passes its checks: {rec.checks}")
rec = Recorder()
smoke.stale_cursor_after(Scripted(lambda t, a: page([{"job_id": 1}])), "vm", rec, "old")
require(rec.failed >= 1, "a cursor that is still accepted fails the stale check")
rec = Recorder()
answers = iter([page([], error="its jobs are gone; call list_jobs without cursor", code="cursor_stale"),
                page([{"job_id": 1}])])
smoke.stale_cursor_after(Scripted(lambda t, a: next(answers)), "vm", rec, "old")
require(rec.failed >= 1, "a message claiming the jobs are gone fails")


# I1: "its processes are gone" must not be satisfiable by zero processes from the start.
def counted(values):
    seq = iter(values)
    last = [values[-1]]

    def handler(tool, args):
        try:
            last[0] = next(seq)
        except StopIteration:
            pass
        return result(state="exited", stdout=last[0])

    return Scripted(handler)


rec = Recorder()
zero = counted(["0"])
require(smoke.processes_present(zero, "vm", rec, "cancel", "count", 3, seconds=0.2, every=0.02) is False
        and rec.failed == 1, "zero processes from the start fail the 'existed before the stop' check")
rec = Recorder()
require(smoke.processes_present(counted(["0", "1", "3"]), "vm", rec, "cancel", "count", 3, seconds=2, every=0.02)
        is True and rec.failed == 0, "processes that appear within the bound are proven")
rec = Recorder()
never = Scripted(lambda t, a: (_ for _ in ()).throw(AssertionError("must not look")))
require(smoke.processes_gone(never, "vm", rec, "cancel", "count", proven=False) is False and rec.failed == 1,
        "'gone' without proof they existed fails, without even looking")
rec = Recorder()
require(smoke.processes_gone(counted(["3", "1", "0"]), "vm", rec, "cancel", "count", proven=True, seconds=2, every=0.02)
        is True and rec.failed == 0, "processes proven present then gone pass")
rec = Recorder()
require(smoke.processes_gone(counted(["3"]), "vm", rec, "cancel", "count", proven=True, seconds=0.2, every=0.02)
        is False and rec.failed == 1, "processes that stay fail")
import inspect  # noqa: E402

source = inspect.getsource(smoke.cancellation_checks)
require(source.count("processes_present(") == 2 and source.count("processes_gone(") == 2,
        "cancellation_checks proves presence before, and absence after, both the cancel and the disconnect")


# M2: a failed background start is a failed check, not a skipped block.
def bg_start(job_id):
    return Scripted(lambda t, a: result(state="started", job_id=job_id) if t == "run_command" else result(state="running"))


rec = Recorder()
smoke.survive_cancel_checks(bg_start(None), "vm", rec, "sleep 1", settle=0)
require(any(not ok and "background" in label for label, ok in rec.checks), "a background start without a job id fails a check")
rec = Recorder()
smoke.survive_cancel_checks(bg_start(7), "vm", rec, "sleep 1", settle=0)
require(rec.checks and rec.failed == 0, f"a background job that survives passes: {rec.checks}")


# I2/I1: the HTTP fixture runs under python3 where the guest has it, else under perl; only a guest
# with neither gets one visible FAILED fixture check, never a quiet skip. The TCP checks that need
# only the guest's own sshd run either way.
def readiness_guest(fixture):
    """A canned guest whose fixture interpreter is "python3", "perl" or None (neither)."""
    def handler(tool, args):
        if tool == "run_command":
            command = args.get("command", "")
            if "command -v python3" in command:  # the fixture detection
                return result(state="exited", stdout=fixture or "none")
            if args.get("background"):
                return result(state="started", job_id=11 if "exit 7" not in command else 12)
            return result(state="exited", execution_id=21, stdout="")
        if tool == "get_job_status":
            return result(state="exited" if args["id"] == 12 else "running")
        if tool == "wait_for_service":
            port, job = args["guest_port"], args.get("job_id")
            if port == smoke.CLOSED_PORT and job == 12:
                return {"isError": True, "content": [{"type": "text", "text": "x"}],
                        "structuredContent": {"state": "job_failed", "attempts": 1}}
            if port == smoke.CLOSED_PORT:
                return {"isError": True, "content": [{"type": "text", "text": "x"}],
                        "structuredContent": {"state": "timed_out", "timed_out": True, "tcp": "refused", "attempts": 3}}
            return result(state="ready", tcp="accepted", criterion_met=True, attempts=2, http_status=200,
                          http_outcome="status_matched")
        return result()

    return Scripted(handler)


def labels(rec):
    return " | ".join(f"{'PASS' if ok else 'FAIL'} {label}" for label, ok in rec.checks)


def background_commands(guest_m):
    return [a["command"] for t, a in guest_m.seen if t == "run_command" and a.get("background")]


rec, guest_m = Recorder(), readiness_guest(None)
smoke.readiness_checks(guest_m, "vm", rec, windows=False)
fixture_failures = [label for label, ok in rec.checks if not ok]
require(len(fixture_failures) == 1 and "fixture" in fixture_failures[0],
        f"with neither python3 nor perl exactly one readiness check fails, naming the fixture: {labels(rec)}")
waited = [a["guest_port"] for t, a in guest_m.seen if t == "wait_for_service"]
require(smoke.SSH_PORT in waited and smoke.CLOSED_PORT in waited and smoke.READY_PORT not in waited,
        f"TCP checks against the guest's sshd still ran, HTTP ones did not: {waited}")
require(any(ok for _, ok in rec.checks), "the TCP checks passed")
for fixture, marker, late_marker in (("python3", "python3 -u", "python3 -c"), ("perl", "IO::Socket::INET", "IO::Socket::INET")):
    rec, guest_m = Recorder(), readiness_guest(fixture)
    smoke.readiness_checks(guest_m, "vm", rec, windows=False)
    require(not any("fixture" in label and not ok for label, ok in rec.checks),
            f"with {fixture} the fixture check passes: {labels(rec)}")
    require(smoke.READY_PORT in [a.get("guest_port") for t, a in guest_m.seen if t == "wait_for_service"],
            f"with {fixture} the HTTP checks run")
    commands = background_commands(guest_m)
    require(any(marker in c and str(smoke.READY_PORT) in c for c in commands),
            f"with {fixture} the HTTP server is started under {fixture}")
    require(any(late_marker in c and str(smoke.READY_PORT + 2) in c for c in commands),
            f"with {fixture} the late listener is started under {fixture}")
    if fixture == "perl":
        require(not any("python3 -u" in c or "python3 -c" in c for c in commands), "perl-only never starts python3")
rec, guest_m = Recorder(), readiness_guest(None)
smoke.readiness_checks(guest_m, "vm", rec, windows=True)
require(not any("fixture" in label and not ok for label, ok in rec.checks), "Windows needs neither python3 nor perl")

# I1: the HTTP and late-listener fixtures themselves, run for real on THIS host (random free
# loopback ports, a throwaway HOME, killed and reaped afterwards). This shows the fixture scripts
# behave as the readiness checks assume; it does not show that a guest has python3 or perl.
import shutil  # noqa: E402
import signal  # noqa: E402
import socket  # noqa: E402
import struct  # noqa: E402
import subprocess  # noqa: E402


def free_port():
    with socket.socket() as s:
        s.bind(("127.0.0.1", 0))
        return s.getsockname()[1]


def accepts(port):
    try:
        with socket.create_connection(("127.0.0.1", port), timeout=1):
            return True
    except OSError:
        return False


def wait_until(fn, seconds=15):
    end = time.monotonic() + seconds
    while time.monotonic() < end:
        if fn():
            return True
        time.sleep(0.05)
    return False


def get(port, path, timeout=10):
    """(status, Location header or None) of one GET, read to the end of the reply."""
    with socket.create_connection(("127.0.0.1", port), timeout=timeout) as c:
        c.sendall(f"GET {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n".encode())
        data = b""
        while chunk := c.recv(4096):
            data += chunk
    head = data.split(b"\r\n\r\n", 1)[0].decode("latin-1").split("\r\n")
    location = next((h.split(": ", 1)[1] for h in head[1:] if h.lower().startswith("location:")), None)
    return int(head[0].split()[1]), location


class Host:
    """A shell command run in its own process group on this host, with its own HOME."""

    def __init__(self, command, home):
        self.proc = subprocess.Popen(["sh", "-c", command], env={**os.environ, "HOME": home}, start_new_session=True,
                                     stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)

    def alive(self):
        return self.proc.poll() is None

    def stop(self):
        try:
            os.killpg(self.proc.pid, signal.SIGKILL)
        except ProcessLookupError:
            pass
        self.proc.wait()
        return wait_until(lambda: not group_exists(self.proc.pid), 5)


def group_exists(pgid):
    try:
        os.killpg(pgid, 0)
        return True
    except ProcessLookupError:
        return False


for kind in ("perl", "python3"):
    if not shutil.which(kind):
        print(f"    (no {kind} on this host: its fixture was not run here)")
        continue
    home = tempfile.mkdtemp(prefix="smoke-fixture-home-")
    port, late_port = free_port(), free_port()
    started = []
    try:
        server = Host(smoke.linux_http_server(kind, port, slow_seconds=1), home)
        started.append(server)
        require(wait_until(lambda: accepts(port)), f"{kind}: the HTTP fixture accepts connections")
        require(get(port, "/")[0] == 200, f"{kind}: / is 200")
        require(get(port, "/mismatch")[0] == 503, f"{kind}: /mismatch is 503")
        status, location = get(port, "/redirect")
        require((status, location) == (302, "/target"), f"{kind}: /redirect is 302 to /target")
        require(get(port, "/Unicode-%C3%A9%20space")[0] == 200, f"{kind}: an escaped Unicode path is 200")
        require(not (Path(home) / "ready-target-count").exists(), f"{kind}: nothing visited the redirect target")
        require(get(port, "/target")[0] == 200 and (Path(home) / "ready-target-count").read_text() == "1",
                f"{kind}: the target counter counts a real visit")
        # TCP-only probes (connect then close), an empty line, a partial request: none may kill it.
        for _ in range(3):
            socket.create_connection(("127.0.0.1", port), timeout=2).close()
        with socket.create_connection(("127.0.0.1", port), timeout=2) as c:
            c.sendall(b"\r\n")
        with socket.create_connection(("127.0.0.1", port), timeout=2) as c:
            c.sendall(b"GE")
        require(get(port, "/")[0] == 200 and server.alive(), f"{kind}: survives empty TCP connects and empty lines")
        # A cancelled slow request: the client goes away before the delayed reply is written.
        c = socket.create_connection(("127.0.0.1", port), timeout=2)
        c.sendall(b"GET /slow HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n")
        time.sleep(0.3)
        # An abortive close (RST), so the delayed write hits a reset socket: SIGPIPE if not ignored.
        c.setsockopt(socket.SOL_SOCKET, socket.SO_LINGER, struct.pack("ii", 1, 0))
        c.close()
        require(get(port, "/", timeout=15)[0] == 200 and server.alive(),
                f"{kind}: survives a cancelled slow response and keeps serving")
        # A late listener: refused at first, accepted after its delay.
        late = Host(smoke.late_listener_command(kind, late_port, delay=1), home)
        started.append(late)
        require(not accepts(late_port), f"{kind}: the late listener is not up before its delay")
        require(wait_until(lambda: accepts(late_port)), f"{kind}: the late listener binds after its delay")
    finally:
        gone = [h.stop() for h in started]
        shutil.rmtree(home, ignore_errors=True)
    require(all(gone), f"{kind}: no fixture process is left behind")
    require(not accepts(port) and not accepts(late_port), f"{kind}: the fixture ports are closed afterwards")

# 6. The VM-free protocol checks against the real server, when asked for and built.
binary = Path(os.environ.get("AGENTPC_BIN", ROOT / "target" / "debug" / "agentpc"))
real_ran = False
if FAKE_ONLY:
    print("    (--fake-only: no agentpc server was started; the real protocol checks were NOT run)")
elif not binary.is_file():
    errors.append(f"no agentpc binary at {binary}: build it, or pass --fake-only to skip the real protocol checks")
else:
    with tempfile.TemporaryDirectory() as home:
        # A fresh, empty state directory: the guard and the server see no VM at all, whatever the
        # user's real ~/.agentpc holds, and KEEP_RUNNING is set explicitly so nothing is reaped.
        os.environ["AGENTPC_HOME"] = home
        os.environ["AGENTPC_KEEP_RUNNING"] = "1"
        smoke.refuse_unrelated_orphans(str(binary))
        report = smoke.Report()
        shown = []
        original = report.check

        def check(label, ok, detail=""):
            shown.append((label, ok))
            return original(label, ok, detail if not ok else "")

        report.check = check
        smoke.protocol_checks(str(binary), report)
    require(report.failed == 0, f"protocol_checks failed {report.failed} check(s)")
    labels_text = " | ".join(label for label, _ in shown)
    for needle in ("list_jobs", "wait_for_service", "cancel"):
        require(needle in labels_text, f"protocol_checks covers {needle}")
    real_ran = True

if errors:
    for error in errors:
        print(f"FAIL: {error}")
    sys.exit(1)
if real_ran:
    print("PASS: smoke.py's concurrent client, recovery helpers and VM-free protocol checks")
else:
    print("PASS (fake-only): the concurrent client and recovery helpers; the real-server protocol checks were NOT run")
