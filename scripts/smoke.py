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

Recovery and readiness checks (every OS, on a concurrent client that can leave requests in
flight): a foreground run cancelled with notifications/cancelled is stopped and found again with
list_jobs and get_job_status, a background job survives its start request's cancellation, a
disconnected second session stops its foreground run, list_jobs pages and degrades gracefully, a
list cursor goes stale across restore_vm, and wait_for_service sees late TCP and HTTP listeners,
status mismatches, an unfollowed redirect, a closed port and the job-failure shortcut without
stopping the server. The VM-free protocol checks cover the four typed tools' schemas and errors.

Execution and observation checks (every OS): the declared tool schemas and annotations over
the real protocol (no VM needed), structured run_command results (streams, exit codes, errors),
foreground timeouts that stop the command's own processes and leave unrelated ones alone,
get_job_status waits and cursors (a wait leaves the job running; Unicode output arrives exactly
once), and inspect_desktop (observes; refuses to act). They print only labels and counts:
`METRIC` lines give calls, result bytes and elapsed seconds, as measurements, not comparisons.
Before it starts an MCP server this script refuses to run while an unrelated VM is running
without an owner, because a new server stops such VMs.
"""

import argparse
import hashlib
import json
import os
import platform
import re
import shutil
import socket
import subprocess
import sys
import tempfile
import threading
import time
import traceback
import urllib.request
from concurrent.futures import Future
from concurrent.futures import TimeoutError as FutureTimeout
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
CALL_TIMEOUT = 900  # create_vm of a Windows image that needs a snapshot can take minutes


# Output hygiene. A lifecycle reply (create_vm, start_vm, reset_vm, restore_vm, checkpoint_vm,
# list_vms) carries the VM's viewer link, whose fragment holds its VNC password. Those replies are
# redacted where they are read, so nothing built from one (a slice in a failure message, a repr,
# a parsed listing) can hold it; redact() is also the last step of every line this script prints
# about a failure, and of tracebacks. test-smoke-logging.py pins this.
LIFECYCLE_TOOLS = {"create_vm", "start_vm", "reset_vm", "restore_vm", "checkpoint_vm", "list_vms"}
REDACTED = "<redacted>"
_URL = re.compile(r"""[A-Za-z][A-Za-z0-9+.-]*://[^\s"'<>]*""")
_VIEWER_PAGE = re.compile(r"""vnc\.html[^\s"'<>]*""")
_FRAGMENT = re.compile(r"""#[^\s"'<>]*=[^\s"'<>]*""")
_CREDENTIAL = re.compile(
    r"""(?i)(pass(?:word|wd)?|token|secret|credential|api[_-]?key)(["']?\s*[=:]\s*)("[^"]*"|'[^']*'|[^\s&"',;}]+)"""
)


def redact(text):
    """text without viewer links, URL fragments or key=value / "key": "value" credentials."""
    text = _URL.sub(REDACTED, str(text))
    text = _VIEWER_PAGE.sub(REDACTED, text)
    text = _FRAGMENT.sub(REDACTED, text)
    return _CREDENTIAL.sub(lambda m: m.group(1) + m.group(2) + REDACTED, text)


def safe_excepthook(kind, value, tb):
    sys.stderr.write(redact("".join(traceback.format_exception(kind, value, tb))))


sys.excepthook = safe_excepthook


class Mcp:
    def __init__(self, binary, env=None):
        self.binary = binary
        self.proc = subprocess.Popen(
            [binary, "mcp"],
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.DEVNULL,
            env={**os.environ, **(env or {})},
        )
        # A reader thread matches each reply to its request by id and completes that request's
        # future, so requests can overlap: one left in flight (to be cancelled, or abandoned)
        # does not hold up the others, and no reply sits unseen in a buffer.
        self.send_lock = threading.Lock()
        self.pending = {}  # request id -> Future of the whole reply message
        self.pending_lock = threading.Lock()
        self.next_id = 0
        self.calls = 0
        self.log = []  # (tool, seconds, result bytes) of every call(), for METRIC lines
        threading.Thread(target=self.read_lines, daemon=True).start()
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
        try:
            for line in self.proc.stdout:
                try:
                    msg = json.loads(line)
                except ValueError:
                    continue
                if "method" in msg or "id" not in msg:  # a notification or a request from the server
                    continue
                with self.pending_lock:
                    fut = self.pending.pop(msg["id"], None)
                if fut is not None and not fut.done():
                    fut.set_result(msg)
        finally:  # EOF: nothing pending will ever be answered
            with self.pending_lock:
                orphaned, self.pending = list(self.pending.values()), {}
            for fut in orphaned:
                if not fut.done():
                    fut.set_exception(RuntimeError("agentpc mcp exited"))

    def send(self, msg):
        with self.send_lock:
            self.proc.stdin.write((json.dumps(msg) + "\n").encode())
            self.proc.stdin.flush()

    def notify(self, method, params=None):
        msg = {"jsonrpc": "2.0", "method": method}
        if params is not None:
            msg["params"] = params
        self.send(msg)

    def start(self, method, params):
        """Send a request without waiting: (its id, a Future of the reply). Pair it with
        finish(), or with cancel() and forget() to abandon it."""
        fut = Future()
        with self.pending_lock:
            self.next_id += 1
            want = self.next_id
            self.pending[want] = fut
        self.send({"jsonrpc": "2.0", "id": want, "method": method, "params": params})
        return want, fut

    def finish(self, method, fut, timeout=None):
        """The result of a start()ed request; raises RuntimeError for a JSON-RPC error or the
        server's exit, TimeoutError past `timeout` (CALL_TIMEOUT by default)."""
        try:
            msg = fut.result(timeout=CALL_TIMEOUT if timeout is None else timeout)
        except FutureTimeout:
            raise TimeoutError(f"{method} took over {CALL_TIMEOUT if timeout is None else timeout}s") from None
        if "error" in msg:
            raise RuntimeError(f"{method}: {msg['error']}")
        return msg["result"]

    def request(self, method, params):
        return self.finish(method, self.start(method, params)[1])

    def cancel(self, request_id, reason="smoke cancellation"):
        """Send notifications/cancelled for a request that is in flight."""
        self.notify("notifications/cancelled", {"requestId": request_id, "reason": reason})

    def forget(self, request_id):
        """Stop waiting for a request: whatever the server sends for it is dropped."""
        with self.pending_lock:
            self.pending.pop(request_id, None)

    def start_call(self, tool_name, /, **args):
        """start() for tools/call: (request id, Future). Finish it with finish("tools/call", fut)."""
        return self.start("tools/call", {"name": tool_name, "arguments": args})

    def call(self, tool_name, /, **args):
        """Call an agentpc tool; returns the whole result: content blocks, isError and
        structuredContent. Held in memory only; print it through brief()."""
        self.calls += 1
        start = time.monotonic()
        res = self.request("tools/call", {"name": tool_name, "arguments": args})
        self.log.append((tool_name, time.monotonic() - start, len(json.dumps(res))))
        return res

    def tool(self, tool_name, /, **args):
        """Call an agentpc tool; returns (ok, text)."""
        res = self.call(tool_name, **args)
        text = text_of(res)
        return not res.get("isError", False), redact(text) if tool_name in LIFECYCLE_TOOLS else text

    def list_tools(self):
        return self.request("tools/list", {})["tools"]

    def measured(self, since):
        """(calls, result bytes, seconds) of the call()s made after `since` (a len(self.log))."""
        part = self.log[since:]
        return len(part), sum(b for _, _, b in part), sum(s for _, s, _ in part)

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
        """End the server's input, wait for it, kill it if it will not go, and reap it and
        release both pipes either way."""
        try:
            self.proc.stdin.close()
        except OSError:
            pass
        try:
            self.proc.wait(timeout=60)
        except subprocess.TimeoutExpired:
            self.proc.kill()
            self.proc.wait()
        try:  # the reader thread ends at EOF; then nothing else holds the pipe
            self.proc.stdout.close()
        except OSError:
            pass


class Report:
    def __init__(self):
        self.failed = 0

    def check(self, label, ok, detail=""):
        shown = redact(detail) if detail and not ok else ""
        print(f"  {'PASS' if ok else 'FAIL'}  {label}" + (f": {shown}" if shown else ""))
        if not ok:
            self.failed += 1
        return ok


def text_of(res):
    """The text blocks of a raw tool result, joined as a text-only client sees them."""
    return "\n".join(c.get("text", "") for c in res.get("content", []) if c.get("type") == "text")


def structured(res):
    return res.get("structuredContent") or {}


def brief(res):
    """A raw result as safe labels for a failure message: typed state fields and block
    counts only, never output text, desktop state or anything a viewer link could be in."""
    d = structured(res)
    keys = ("state", "exit_code", "timed_out", "termination_confirmed", "effect_unknown",
            "recovery_action", "error_code", "cursor_reset", "cursor_reset_reason",
            "wait_timed_out", "has_more", "request_cancelled", "scan_truncated", "http_outcome",
            "http_status", "tcp", "attempts")
    out = {k: d[k] for k in keys if k in d}
    out["isError"] = bool(res.get("isError"))
    out["blocks"] = [c.get("type") for c in res.get("content", [])]
    return str(out)


def metric(label, m, since):
    """Print what the calls since `since` cost: a measurement, not a comparison."""
    calls, size, secs = m.measured(since)
    print(f"  METRIC  {label}: {calls} calls, {size} result bytes, {secs:.2f}s")


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


# ---- Execution and observation -------------------------------------------------------------
# Nothing below prints command output, desktop state or protocol replies: failures show
# brief() labels only.

RUN_COMMAND_FIELDS = {
    "schema_version", "state", "execution_id", "job_id", "background", "exit_code", "stdout",
    "stderr", "stdout_bytes", "stderr_bytes", "stdout_truncated", "stderr_truncated",
    "output_truncated", "timed_out", "termination_confirmed", "effect_unknown",
    "recovery_action", "log_path", "error", "request_cancelled",
}
RUN_COMMAND_STATES = {"exited", "timed_out", "not_started", "connection_lost", "started", "cancelled"}
JOB_STATUS_FIELDS = {
    "schema_version", "state", "job_id", "kind", "exit_code", "stop_reason", "log_available",
    "log_tail", "output", "output_bytes", "output_truncated", "invalid_utf8", "has_more",
    "next_cursor", "cursor_reset", "cursor_reset_reason", "wait_timed_out", "waited_ms",
    "note", "error", "error_code",
}
JOB_STATES = {"queued", "running", "exited", "stopped", "ended", "not_started", "missing", "unknown"}
LIST_JOBS_FIELDS = {
    "schema_version", "jobs", "limit", "has_more", "continuation", "next_cursor", "listed_at",
    "scan_truncated", "note", "error", "error_code",
}
WAIT_FIELDS = {
    "schema_version", "state", "name", "protocol", "guest_port", "path", "expected_status",
    "criterion", "criterion_met", "tcp", "http_outcome", "http_status", "attempts", "waited_ms",
    "timeout_seconds", "timed_out", "cancelled", "job", "note", "error", "error_code",
}
WAIT_STATES = {"ready", "timed_out", "job_failed", "cancelled", "error"}
TYPED_TOOLS = {"run_command", "get_job_status", "list_jobs", "wait_for_service"}
MAX_LIST_LIMIT = 100
MAX_WAIT_TIMEOUT = 120
MAX_REPLY_BYTES = 16 * 1024  # most log bytes one get_job_status reply carries
MAX_WAIT_SECONDS = 30
# Most a foreground timeout may take beyond `timeout`: the cancellation, session and drain
# bounds add up to about 52 s in the worst case.
CLEANUP_BOUND = 60


def documented_tools():
    """Tool names in the first column of AGENTS.md's "Using the VMs" table (as check.sh reads it)."""
    section = (ROOT / "AGENTS.md").read_text().split("## Using the VMs", 1)[1].split("\n## ", 1)[0]
    names = set()
    for line in section.splitlines():
        if line.startswith("| `"):
            names.update(re.findall(r"`([a-z_]+)[`(]", line.split("|")[1]))
    return names


def enum_values(schema):
    """Every string a JSON-schema node allows through enum / const / oneOf / anyOf."""
    found = set(v for v in schema.get("enum", []) if isinstance(v, str))
    if isinstance(schema.get("const"), str):
        found.add(schema["const"])
    for key in ("oneOf", "anyOf"):
        for sub in schema.get(key, []):
            found |= enum_values(sub)
    return found


def refuse_unrelated_orphans(binary):
    """A new MCP server stops running VMs whose server is gone. Don't start one while such a
    VM isn't ours (a smoke VM an earlier run left behind is ours: reap_stale_vms deletes it)."""
    try:
        listing = json.loads(subprocess.run(
            [binary, "list", "--json"], capture_output=True, text=True, timeout=60, check=True,
        ).stdout)
    except (OSError, ValueError, subprocess.SubprocessError) as e:
        sys.exit(f"cannot list VMs before starting an MCP server: {type(e).__name__}")
    unrelated = [
        v["name"] for v in listing.get("instances", [])
        if v.get("state") != "stopped" and v.get("owner_running") is False
        and not (v["name"].startswith("smoke-") and (v.get("owner") or "").startswith("agentpc-smoke ["))
    ]
    if unrelated:
        sys.exit("refusing to start an MCP server: it would stop these running VMs whose own "
                 f"server is gone: {', '.join(unrelated)}")


def protocol_checks(binary, r):
    """What the server declares and answers over the real protocol, with no VM: the tool
    list against AGENTS.md, output schemas, input bounds, annotations, and the errors a
    client gets for a VM that doesn't exist."""
    m = Mcp(binary)
    try:
        tools = {t["name"]: t for t in m.list_tools()}
        documented = documented_tools()
        r.check("the server lists exactly the tools AGENTS.md documents", set(tools) == documented,
                f"differ: {sorted(set(tools) ^ documented)}")
        declared = {n for n, t in tools.items() if t.get("outputSchema")}
        r.check("only run_command, get_job_status, list_jobs and wait_for_service declare an output schema",
                declared == TYPED_TOOLS, str(sorted(declared)))
        for name, fields, states in (
            ("run_command", RUN_COMMAND_FIELDS, RUN_COMMAND_STATES),
            ("get_job_status", JOB_STATUS_FIELDS, JOB_STATES),
        ):
            schema = tools[name].get("outputSchema") or {}
            props = schema.get("properties", {})
            r.check(f"{name}: the output schema is an object with the documented fields and no $ref",
                    schema.get("type") == "object" and fields <= set(props)
                    and "$ref" not in json.dumps(schema), f"missing: {sorted(fields - set(props))}")
            r.check(f"{name}: the output schema's states are the documented ones",
                    enum_values(props.get("state", {})) == states, str(sorted(enum_values(props.get("state", {})))))
        for name, fields, states in (
            ("list_jobs", LIST_JOBS_FIELDS, None),
            ("wait_for_service", WAIT_FIELDS, WAIT_STATES),
        ):
            schema = tools[name].get("outputSchema") or {}
            props = schema.get("properties", {})
            r.check(f"{name}: the output schema is an object with the documented fields",
                    schema.get("type") == "object" and fields <= set(props), f"missing: {sorted(fields - set(props))}")
            if states is not None:
                r.check(f"{name}: the output schema's states are the documented ones",
                        enum_values(props.get("state", {})) == states, str(sorted(enum_values(props.get("state", {})))))
        job_in = tools["get_job_status"]["inputSchema"].get("properties", {})
        wait = job_in.get("wait_seconds", {})
        r.check("get_job_status input: wait_seconds is bounded 0..30, tail_lines and cursor remain",
                wait.get("minimum") == 0 and wait.get("maximum") == MAX_WAIT_SECONDS
                and {"tail_lines", "cursor"} <= set(job_in)
                and tools["get_job_status"]["inputSchema"].get("required") == ["name", "id"],
                str(wait))

        def note(n):
            return tools[n].get("annotations") or {}

        r.check("run_command and use_desktop_tool are destructive and open-world",
                all(note(n).get("destructiveHint") is True and note(n).get("readOnlyHint") is False
                    and note(n).get("openWorldHint") is True for n in ("run_command", "use_desktop_tool")))
        r.check("inspect_desktop is read-only, not destructive and open-world",
                note("inspect_desktop").get("readOnlyHint") is True
                and note("inspect_desktop").get("destructiveHint") is False
                and note("inspect_desktop").get("openWorldHint") is True, str(note("inspect_desktop")))
        r.check("inspect_desktop takes exactly use_desktop_tool's arguments",
                tools["inspect_desktop"]["inputSchema"] == tools["use_desktop_tool"]["inputSchema"])
        r.check("get_job_status is read-only", note("get_job_status").get("readOnlyHint") is True)
        r.check("list_jobs and wait_for_service are read-only, not destructive, idempotent and closed-world",
                all(note(n).get("readOnlyHint") is True and note(n).get("destructiveHint") is False
                    and note(n).get("idempotentHint") is True and note(n).get("openWorldHint") is False
                    for n in ("list_jobs", "wait_for_service")), str({n: note(n) for n in ("list_jobs", "wait_for_service")}))
        list_in = tools["list_jobs"]["inputSchema"].get("properties", {})
        wait_in = tools["wait_for_service"]["inputSchema"].get("properties", {})
        r.check("list_jobs input: limit is bounded 1..100 (default 20); only name is required",
                list_in.get("limit", {}).get("minimum") == 1 and list_in.get("limit", {}).get("maximum") == MAX_LIST_LIMIT
                and list_in.get("limit", {}).get("default") == 20
                and tools["list_jobs"]["inputSchema"].get("required") == ["name"], str(list_in.get("limit")))
        r.check("wait_for_service input: port 1..65535, timeout_seconds 1..120 (default 30), no default for path or expected_status",
                wait_in.get("guest_port", {}).get("minimum") == 1 and wait_in.get("guest_port", {}).get("maximum") == 65535
                and wait_in.get("timeout_seconds", {}).get("maximum") == MAX_WAIT_TIMEOUT
                and wait_in.get("timeout_seconds", {}).get("default") == 30
                and "default" not in wait_in.get("path", {}) and "default" not in wait_in.get("expected_status", {})
                and tools["wait_for_service"]["inputSchema"].get("required") == ["name", "guest_port"],
                str(sorted(wait_in)))
        r.check("run_command's description states cancellation and where to recover a cancelled run",
                all(term in (tools["run_command"].get("description") or "")
                    for term in ("cancelled", "request_cancelled", "list_jobs", "background")), "")

        ghost = f"smoke-absent-{os.getpid()}"
        res = m.call("run_command", name=ghost, command="echo never")
        d, text = structured(res), text_of(res)
        r.check("run_command on a missing VM: tool error, typed not_started, legacy one-block text",
                res.get("isError") is True and d.get("state") == "not_started"
                and d.get("execution_id") is None and len(res["content"]) == 1
                and text.startswith("no instance") and not text.lstrip().startswith("{"), brief(res))
        res = m.call("run_command", name=ghost, command="echo never", background=True)
        r.check("a background run_command on a missing VM says it was background and not started",
                res.get("isError") is True and structured(res).get("background") is True
                and structured(res).get("state") == "not_started", brief(res))
        for label, code, args in (
            ("wait_seconds above 30", "invalid_wait_seconds", {"wait_seconds": MAX_WAIT_SECONDS + 1}),
            ("a malformed cursor", "invalid_cursor", {"cursor": "not-a-cursor"}),
            ("a valid start cursor, then the missing VM", "no_such_vm", {"cursor": "start"}),
        ):
            res = m.call("get_job_status", name=ghost, id=1, **args)
            d = structured(res)
            r.check(f"get_job_status with {label}: structured error {code}",
                    res.get("isError") is True and d.get("error_code") == code
                    and d.get("state") == "unknown" and bool(d.get("error")), brief(res))
        # list_jobs and wait_for_service refuse bad arguments before any VM work, with typed errors.
        for label, code, args in (
            ("limit 0", "invalid_limit", {"limit": 0}),
            (f"limit {MAX_LIST_LIMIT + 1}", "invalid_limit", {"limit": MAX_LIST_LIMIT + 1}),
            ("a malformed cursor", "invalid_cursor", {"cursor": "not-a-cursor"}),
            ("the start cursor of get_job_status", "invalid_cursor", {"cursor": "start"}),
            ("a valid request, then the missing VM", "no_such_vm", {}),
        ):
            res = m.call("list_jobs", name=ghost, **args)
            d = structured(res)
            r.check(f"list_jobs with {label}: structured error {code}, no jobs, one text block",
                    res.get("isError") is True and d.get("error_code") == code and d.get("jobs") == []
                    and len(res["content"]) == 1 and not text_of(res).lstrip().startswith("{"), brief(res))
        for label, code, args in (
            ("port 0", "invalid_port", {"guest_port": 0}),
            ("port 65536", "invalid_port", {"guest_port": 65536}),
            ("protocol udp", "invalid_protocol", {"guest_port": 80, "protocol": "udp"}),
            ("timeout_seconds 121", "invalid_timeout", {"guest_port": 80, "timeout_seconds": MAX_WAIT_TIMEOUT + 1}),
            ("a URL as the path", "invalid_path", {"guest_port": 80, "protocol": "http", "path": "//host/x"}),
            ("expected_status 99", "invalid_status", {"guest_port": 80, "protocol": "http", "expected_status": 99}),
            ("a path with tcp", "invalid_argument", {"guest_port": 80, "path": "/x"}),
            ("a valid request, then the missing VM", "no_such_vm", {"guest_port": 80}),
        ):
            res = m.call("wait_for_service", name=ghost, **args)
            d = structured(res)
            r.check(f"wait_for_service with {label}: structured error {code}, nothing probed, one text block",
                    res.get("isError") is True and d.get("error_code") == code and d.get("state") == "error"
                    and d.get("attempts") == 0 and len(res["content"]) == 1
                    and not text_of(res).lstrip().startswith("{"), brief(res))
    finally:
        m.close()


def observation(m, vm, r, windows=False):
    """Everything about running commands, watching jobs and observing the desktop on `vm`."""
    execution_checks(m, vm, r, windows)
    job_checks(m, vm, r, windows)
    recovery_checks(m, vm, r, windows)
    inspect_checks(m, vm, r, windows)


def execution_checks(m, vm, r, windows):
    since = len(m.log)

    def run(command, **kw):
        return m.call("run_command", name=vm, command=command, **kw)

    def status(job, **kw):
        res = m.call("get_job_status", name=vm, id=job, **kw)
        return res, structured(res)

    def out(res):
        return structured(res).get("stdout", "").strip()

    # A success: typed fields, separate streams, and the text a script has always parsed.
    res = run("[Console]::Out.WriteLine('hello'); [Console]::Error.WriteLine('oops')" if windows
              else "printf 'hello\\n'; printf 'oops\\n' >&2", timeout=20)
    d, text = structured(res), text_of(res)
    r.check("run_command success: typed state, code and separate streams, no job handle needed",
            not res.get("isError") and d.get("state") == "exited" and d.get("exit_code") == 0
            and d.get("stdout") == "hello" and d.get("stderr") == "oops"
            and d.get("execution_id") is not None and d.get("job_id") is None
            and d.get("timed_out") is False and d.get("effect_unknown") is False
            and d.get("termination_confirmed") is None and d.get("output_truncated") is False,
            brief(res))
    r.check("run_command success: one legacy text block (exit code, stdout section), not JSON",
            len(res["content"]) == 1 and text.startswith("exit code: 0")
            and stdout(text) == "hello" and "--- stderr ---" in text
            and not text.lstrip().startswith("{"), brief(res))

    # Nonzero exits are tool errors that keep the command's own code, including the codes the
    # wrapper itself uses before a command starts, and (Windows) codes SSH folds to 8 bits.
    codes = [3, 97, 98, 255] + ([-1, 511] if windows else [])
    for code in codes:
        res = run(f"exit {code}", timeout=20)
        d = structured(res)
        r.check(f"run_command: exit {code} is an error that reports the command's own code ({code & 255})",
                res.get("isError") is True and d.get("state") == "exited"
                and d.get("exit_code") == code & 255 and d.get("effect_unknown") is False
                and d.get("timed_out") is False and text_of(res).startswith(f"exit code: {code & 255}"),
                brief(res))
        # The foreground handle reports the same code. On Windows only the codes SSH folds
        # to 8 bits are checked this way: those are the ones whose record the reply itself
        # depended on.
        if d.get("execution_id") is not None and (not windows or code in (-1, 511)):
            st, sd = status(d["execution_id"])
            r.check(f"get_job_status on that foreground handle: exited {code & 255}, no log to replay",
                    not st.get("isError") and sd.get("kind") == "foreground" and sd.get("state") == "exited"
                    and sd.get("exit_code") == code & 255 and sd.get("log_tail") is None
                    and sd.get("output") is None, brief(st))

    # A foreground timeout: stops what the command started, keeps the partial streams,
    # confirms the stop, and leaves a process it did not start alone.
    if windows:
        run("Invoke-CimMethod Win32_Process -MethodName Create -Arguments "
            "@{ CommandLine = 'ping.exe -n 4814 127.0.0.1' } | Out-Null", timeout=30)
        payload = ("[Console]::Out.WriteLine('partial'); [Console]::Error.WriteLine('partial-error'); "
                   "Start-Process ping.exe -ArgumentList '-n 4811 127.0.0.1' -WindowStyle Hidden; Start-Sleep 900")
        count = lambda n: ("@(Get-CimInstance Win32_Process | Where-Object "
                           f"{{ $_.CommandLine -like '*-n {n} *' }}).Count")
        mine, unrelated = count(4811), count(4814)
        unrelated_job = None
    else:
        res = run("sleep 4814", background=True)
        unrelated_job = structured(res).get("job_id")
        r.check("an unrelated background process is running before the timeouts",
                unrelated_job is not None, brief(res))
        payload = ("printf 'partial\\n'; printf 'partial-error\\n' >&2; sleep 4811 & sudo -n sleep 4812 & "
                   "env -i setsid sh -c 'trap \"\" TERM; exec sleep 4813' & wait")
        mine = "pgrep -fx 'sleep 481[123]' | wc -l"
        unrelated = "pgrep -fx 'sleep 4814' | wc -l"
        leftover = "pgrep -fx 'sleep 4815' | wc -l"
    try:
        timeout = 2
        began = time.monotonic()
        res = run(payload, timeout=timeout)
        took = time.monotonic() - began
        d, text = structured(res), text_of(res)
        r.check("a foreground timeout is a tool error with a typed, confirmed termination",
                res.get("isError") is True and d.get("state") == "timed_out" and d.get("timed_out") is True
                and d.get("termination_confirmed") is True and d.get("effect_unknown") is False
                and d.get("exit_code") is None and d.get("job_id") is None
                and d.get("recovery_action") is None, brief(res))
        r.check("a foreground timeout keeps the partial stdout and stderr, apart, in structure and text",
                "partial" in d.get("stdout", "") and "partial-error" not in d.get("stdout", "")
                and "partial-error" in d.get("stderr", "")
                and text.startswith(f"timed out after {timeout}s") and "(partial output below)" in text,
                brief(res))
        r.check(f"the timeout reply arrives within its cleanup bound ({took:.1f}s of {timeout + CLEANUP_BOUND}s)",
                took < timeout + CLEANUP_BOUND)
        r.check("the timed-out command's own processes are gone", out(run(mine, timeout=30)) == "0")
        r.check("a process the command did not start is still running", out(run(unrelated, timeout=30)) == "1")
        if d.get("execution_id") is not None:
            st, sd = status(d["execution_id"])
            r.check("get_job_status on that handle: foreground, stopped at the timeout, no log to replay",
                    not st.get("isError") and sd.get("kind") == "foreground" and sd.get("state") == "stopped"
                    and sd.get("stop_reason") == "timeout" and sd.get("log_tail") is None
                    and sd.get("output") is None and "run_command timeout" in text_of(st), brief(st))

        if not windows:
            # A command that finished but left a child holding its output open: its own code
            # survives, the call is still a timeout error, and the child is stopped.
            res = run("sleep 4815 & printf 'payload-completed\\n'", timeout=timeout)
            d = structured(res)
            r.check("a finished command whose child held its output: own exit code kept, still a timeout error",
                    res.get("isError") is True and d.get("state") == "exited" and d.get("exit_code") == 0
                    and d.get("timed_out") is True and d.get("termination_confirmed") is True
                    and d.get("effect_unknown") is False and "payload-completed" in d.get("stdout", ""),
                    brief(res))
            r.check("that child was stopped", out(run(leftover, timeout=30)) == "0")
    finally:
        if unrelated_job is not None:
            m.call("stop_job", name=vm, id=unrelated_job)
        elif windows:
            run("Get-CimInstance Win32_Process | Where-Object { $_.CommandLine -like '*-n 4814 *' } | "
                "ForEach-Object { Stop-Process -Id $_.ProcessId -Force }", timeout=30)
    metric("run_command workflow (streams, exit codes, a timeout and its checks)", m, since)


def job_checks(m, vm, r, windows):
    since = len(m.log)

    def status(job, **kw):
        began = time.monotonic()
        res = m.call("get_job_status", name=vm, id=job, **kw)
        return res, structured(res), time.monotonic() - began

    def start(command):
        res = m.call("run_command", name=vm, command=command, background=True)
        d = structured(res)
        legacy = re.search(r"\(id (\d+)", text_of(res))
        r.check("background run_command: typed job id, log path, and the legacy 'id N' text",
                not res.get("isError") and d.get("state") == "started" and d.get("background") is True
                and d.get("job_id") is not None and d.get("job_id") == d.get("execution_id")
                and bool(d.get("log_path")) and d.get("recovery_action") == "get_job_status"
                and legacy is not None and int(legacy.group(1)) == d.get("job_id"), brief(res))
        return d.get("job_id")

    guest_log = (lambda j: f'"$env:USERPROFILE\\agentpc-bg\\{j}.log"') if windows else (lambda j: f"~/agentpc-bg/{j}.log")
    # Unicode in three chunks, seconds apart.
    job = start("'alpha α'; Start-Sleep 3; 'beta 😀'; Start-Sleep 3; 'gamma'" if windows
                else "printf 'alpha α\\n'; sleep 3; printf 'beta 😀\\n'; sleep 3; printf 'gamma\\n'")
    if job is None:
        return
    res, d, _ = status(job, cursor="start")
    r.check("cursor 'start': a valid state and a continuation cursor",
            not res.get("isError") and d.get("state") in ("queued", "running", "exited")
            and bool(d.get("next_cursor")) and d.get("cursor_reset") is False, brief(res))
    chunks, cursor = [d.get("output") or ""], d.get("next_cursor")
    resets, oversize = 0, 0
    deadline = time.monotonic() + 60
    while time.monotonic() < deadline:
        res, d, _ = status(job, cursor=cursor, wait_seconds=5)
        if res.get("isError"):
            r.check("cursor continuation does not fail", False, brief(res))
            break
        resets += bool(d.get("cursor_reset"))
        oversize += len((d.get("output") or "").encode()) > MAX_REPLY_BYTES
        chunks.append(d.get("output") or "")
        cursor = d.get("next_cursor")
        if d.get("state") == "exited" and not d.get("has_more"):
            break
    seen = "".join(chunks)
    r.check("cursor reads never reset while the log only grows, and stay within the reply cap",
            resets == 0 and oversize == 0, f"resets={resets} oversize={oversize}")
    r.check("Unicode output reaches the client exactly once, in order, with no BOM",
            seen.count("alpha α") == 1 and seen.count("beta 😀") == 1 and seen.count("gamma") == 1
            and seen.index("alpha") < seen.index("beta") < seen.index("gamma") and "﻿" not in seen,
            f"{len(seen)} chars")
    res, d, _ = status(job, cursor=cursor)
    r.check("a cursor at the end returns nothing new and no reset",
            not res.get("isError") and d.get("output") == "" and d.get("cursor_reset") is False
            and d.get("state") == "exited" and d.get("exit_code") == 0, brief(res))
    # No cursor, no wait: the tail every existing consumer reads.
    res, d, _ = status(job)
    text = text_of(res)
    r.check("without cursor or wait: the legacy STATE line and log tail, plus typed fields",
            not res.get("isError") and d.get("state") == "exited" and d.get("exit_code") == 0
            and d.get("wait_timed_out") is False and "gamma" in (d.get("log_tail") or "")
            and "STATE: exited 0" in text and "--- log tail ---" in text, brief(res))
    tail_cursor = d.get("next_cursor")
    res, d, _ = status(job, cursor=tail_cursor)
    r.check("the tail's next_cursor sits at the end of what the tail read",
            bool(tail_cursor) and not res.get("isError") and d.get("output") == ""
            and d.get("cursor_reset") is False, brief(res))
    res, d, _ = status(job + 1, cursor=cursor)
    r.check("a cursor from another job is rejected before anything is read",
            res.get("isError") is True and d.get("error_code") == "invalid_cursor", brief(res))
    res, d, _ = status(job, wait_seconds=MAX_WAIT_SECONDS + 1)
    r.check("wait_seconds above 30 is a structured error, not a long wait",
            res.get("isError") is True and d.get("error_code") == "invalid_wait_seconds", brief(res))

    # A wait that expires leaves the job running and writes nothing into the VM.
    waiting = start("Start-Sleep 900" if windows else "sleep 900")
    if waiting is not None:
        res, d, took = status(waiting, wait_seconds=2)
        r.check("a wait that expires returns the running job with wait_timed_out, having really waited",
                not res.get("isError") and d.get("state") in ("running", "queued") and d.get("wait_timed_out") is True
                and 1.5 <= took < 15 and (d.get("waited_ms") or 0) >= 1500, f"{took:.1f}s {brief(res)}")
        res, d, _ = status(waiting)
        r.check("the job is still running after that wait", d.get("state") in ("running", "queued"), brief(res))
        marker = (f'[IO.File]::Exists("$env:USERPROFILE\\agentpc-bg\\{waiting}.cancel")' if windows
                  else f"test -e ~/agentpc-bg/{waiting}.cancel && echo yes || echo no")
        res = m.call("run_command", name=vm, command=marker, timeout=30)
        r.check("waiting left no cancellation marker behind",
                structured(res).get("stdout", "").strip() == ("False" if windows else "no"), brief(res))
        ok, _ = m.tool("stop_job", name=vm, id=waiting)
        r.check("an explicit stop_job still ends it", ok, "")

    # A wait with a cursor comes back when output arrives, not when the wait ends.
    tick = start("'one'; Start-Sleep 5; 'two'" if windows
                 else "printf 'one\\n'; sleep 5; printf 'two\\n'; sleep 40")
    if tick is not None:
        seen, cursor, deadline = "", "start", time.monotonic() + 20
        while "one" not in seen and time.monotonic() < deadline:
            res, d, _ = status(tick, cursor=cursor, wait_seconds=3)
            if res.get("isError"):
                break
            seen += d.get("output") or ""
            cursor = d.get("next_cursor") or cursor
            if d.get("state") == "exited":
                break
        if "two" not in seen:
            res, d, took = status(tick, cursor=cursor, wait_seconds=25)
            seen += d.get("output") or ""
            r.check("a cursor wait returns when new output arrives, well before it would expire",
                    not res.get("isError") and "two" in (d.get("output") or "")
                    and d.get("wait_timed_out") is False and took < 20, f"{took:.1f}s {brief(res)}")
        r.check("both lines of that job arrived exactly once", seen.count("one") == 1 and seen.count("two") == 1,
                f"{len(seen)} chars")
        m.call("stop_job", name=vm, id=tick)

    # A log that is rewritten in place resets the cursor, said so, and restarts at its beginning.
    res, d, _ = status(job, cursor="start")
    old = d.get("next_cursor")
    rewrite = (f"[IO.File]::WriteAllText({guest_log(job)}, 'replacement', [Text.UTF8Encoding]::new($false))"
               if windows else f"printf 'replacement' > {guest_log(job)}")
    m.call("run_command", name=vm, command=rewrite, timeout=30)
    res, d, _ = status(job, cursor=old)
    r.check("a log truncated and rewritten resets the cursor explicitly and returns it from the start",
            not res.get("isError") and d.get("cursor_reset") is True
            and d.get("cursor_reset_reason") in ("log_shrunk", "log_rewritten", "log_replaced")
            and d.get("output") == "replacement", brief(res))
    metric("job workflow (start, cursor reads with waits, expiry, reset)", m, since)


# ---- Recovery and readiness ------------------------------------------------------------------
# Cancelled and dropped requests, finding jobs again, and waiting for a service, driven over the
# real protocol with requests left in flight. Every payload here is a known synthetic command;
# failures print brief() labels only, never a reply's text.

READY_PORT = 18082
CLOSED_PORT = 18083
# A guest HTTP server for the readiness checks: 200 everywhere except /mismatch (503), /redirect
# (302 to /target, whose requests it counts), and /slow (a reply after 8 s). Started as a
# background job; Python 3 in the Linux guests, a raw listener in PowerShell on Windows.
_PYTHON_HTTP = r"""import http.server, time
from pathlib import Path
class H(http.server.BaseHTTPRequestHandler):
    def do_GET(self):
        if self.path == '/target':
            p = Path.home() / 'ready-target-count'
            p.write_text(str(int(p.read_text()) + 1 if p.exists() else 1))
        if self.path == '/slow':
            time.sleep(__SLOW__)
        code = 503 if self.path == '/mismatch' else 302 if self.path == '/redirect' else 200
        self.send_response(code)
        if code == 302:
            self.send_header('Location', '/target')
        body = b'SYNTHETIC-RESPONSE-BODY-NOT-FOR-RESULT'
        self.send_header('Content-Length', str(len(body)))
        self.end_headers()
        try:
            self.wfile.write(body)
        except (BrokenPipeError, ConnectionResetError):
            pass
    def log_message(self, *args):
        pass
http.server.ThreadingHTTPServer(('127.0.0.1', __PORT__), H).serve_forever()"""
# The same routes under perl's IO::Socket::INET (the idiom of the forward_port check below), for
# guests without python3 (Arch). Single-threaded, so it must survive what the readiness probes do:
# SIGPIPE is ignored (a cancelled /slow reply writes to a closed socket), and a connection that
# sends nothing, an empty line or a partial line is skipped, not served and not fatal.
_PERL_HTTP = r"""use IO::Socket::INET;
$SIG{PIPE} = 'IGNORE';
my $s = IO::Socket::INET->new(LocalAddr => '127.0.0.1', LocalPort => __PORT__, Listen => 16, ReuseAddr => 1)
    or die "listen: $!";
while (my $c = $s->accept) {
    my $line = <$c>;
    next unless defined $line && $line =~ m{^GET (\S+) };
    my $path = $1;
    while (my $h = <$c>) { last if $h =~ /^\r?\n$/ }
    if ($path eq '/target') {
        my $f = "$ENV{HOME}/ready-target-count";
        my $n = 0;
        if (open(my $in, '<', $f)) { $n = (<$in> // 0) + 0; close $in }
        if (open(my $out, '>', $f)) { print $out $n + 1; close $out }
    }
    sleep __SLOW__ if $path eq '/slow';
    my ($code, $reason, $extra) = (200, 'OK', '');
    ($code, $reason) = (503, 'Unavailable') if $path eq '/mismatch';
    ($code, $reason, $extra) = (302, 'Found', "Location: /target\r\n") if $path eq '/redirect';
    my $body = 'SYNTHETIC-RESPONSE-BODY-NOT-FOR-RESULT';
    print $c "HTTP/1.1 $code $reason\r\n${extra}Content-Length: " . length($body) . "\r\nConnection: close\r\n\r\n$body";
}"""


def linux_http_server(kind, port=READY_PORT, slow_seconds=8):
    """The guest HTTP fixture as a shell command: 200 everywhere except /mismatch (503), /redirect
    (302 to /target, whose visits it counts in ~/ready-target-count) and /slow (replies after
    `slow_seconds`). `kind` is "python3" or "perl"; both listen on 127.0.0.1 only and return the
    same synthetic, non-credential body."""
    program, interpreter = (_PYTHON_HTTP, "python3 -u -") if kind == "python3" else (_PERL_HTTP, "perl -")
    program = program.replace("__PORT__", str(port)).replace("__SLOW__", str(slow_seconds))
    return f"exec {interpreter} <<'SERVER'\n{program}\nSERVER"


def late_listener_command(kind, port, delay=4):
    """A TCP listener on 127.0.0.1:port that binds only after `delay` seconds and holds it."""
    if kind == "python3":
        listen = ("python3 -c \"import socket,time; s=socket.socket(); "
                  "s.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1); "
                  f"s.bind(('127.0.0.1', {port})); s.listen(); time.sleep(600)\"")
    else:
        listen = ("perl -MIO::Socket::INET -e '$s = IO::Socket::INET->new(LocalAddr => \"127.0.0.1\", "
                  f"LocalPort => {port}, Listen => 5, ReuseAddr => 1) or die; sleep 600'")
    return f"sleep {delay}; exec {listen}"


WINDOWS_HTTP_SERVER = r"""$l = [Net.Sockets.TcpListener]::new([Net.IPAddress]::Loopback, %d); $l.Start()
while ($true) {
  $cl = $l.AcceptTcpClient(); $st = $cl.GetStream(); $st.ReadTimeout = 2000; $st.WriteTimeout = 2000
  try {
    $rd = [IO.StreamReader]::new($st); $line = $rd.ReadLine()
    if (!$line) { continue }
    $path = ($line -split ' ')[1]
    while ($rd.ReadLine()) {}
    if ($path -eq '/target') {
      $f = "$env:USERPROFILE\ready-target-count"; $n = 0
      if (Test-Path $f) { $n = [int][IO.File]::ReadAllText($f) }
      [IO.File]::WriteAllText($f, [string]($n + 1))
    }
    if ($path -eq '/slow') { Start-Sleep 8 }
    $code = 200; $reason = 'OK'; $extra = ''
    if ($path -eq '/mismatch') { $code = 503; $reason = 'Unavailable' }
    if ($path -eq '/redirect') { $code = 302; $reason = 'Found'; $extra = "Location: /target`r`n" }
    $body = 'SYNTHETIC-RESPONSE-BODY-NOT-FOR-RESULT'
    $bytes = [Text.Encoding]::ASCII.GetBytes("HTTP/1.1 $code $reason`r`n${extra}Content-Length: $($body.Length)`r`nConnection: close`r`n`r`n$body")
    $st.Write($bytes, 0, $bytes.Length)
  } catch {} finally { $cl.Close() }
}""" % READY_PORT


def guest(m, vm, command, **kw):
    """run_command's typed result for `command`, as a dict (stdout etc.)."""
    kw.setdefault("timeout", 30)
    return structured(m.call("run_command", name=vm, command=command, **kw))


def guest_out(m, vm, command, **kw):
    return guest(m, vm, command, **kw).get("stdout", "").strip()


def list_all_jobs(m, vm, limit=100, pages=50):
    """Every job list_jobs returns, following next_cursor; None if a page is an error."""
    jobs, cursor = [], None
    for _ in range(pages):
        args = {"limit": limit}
        if cursor:
            args["cursor"] = cursor
        res = m.call("list_jobs", name=vm, **args)
        d = structured(res)
        if res.get("isError"):
            return None
        jobs += d.get("jobs", [])
        cursor = d.get("next_cursor")
        if not d.get("has_more") or not cursor:
            return jobs
    return jobs


def job_state(m, vm, job):
    return structured(m.call("get_job_status", name=vm, id=job)).get("state")


def processes_present(m, vm, r, label, count_command, expected, seconds=40, every=1):
    """Prove, boundedly, that `expected` processes exist (count_command prints their number) before
    a stop is asked for: a marker file shows only that a payload began, not that it spawned them."""
    ok = bool(poll(lambda: guest_out(m, vm, count_command) == str(expected), seconds, every))
    r.check(f"{label}: {expected} process(es) exist before the stop", ok,
            "the expected processes never appeared, so a later count of 0 would prove nothing")
    return ok


def processes_gone(m, vm, r, label, count_command, proven, seconds=30, every=1):
    """After a stop: none are left. Without `proven` presence beforehand this is a failed check
    (it never looks), because zero from the start is not evidence that anything was stopped."""
    if not proven:
        r.check(f"{label}: gone after the stop", False, "their existence was never proven, so this cannot pass")
        return False
    ok = bool(poll(lambda: guest_out(m, vm, count_command) == "0", seconds, every))
    r.check(f"{label}: gone after the stop", ok)
    return ok


def survive_cancel_checks(m, vm, r, command, settle=2):
    """A background start is detached: cancelling its (completed) request does not stop the job."""
    rid, fut = m.start_call("run_command", name=vm, command=command, background=True)
    started = m.finish("tools/call", fut)
    bg = structured(started).get("job_id")
    r.check("the background start returned a job id", bg is not None, brief(started))
    m.cancel(rid)
    time.sleep(settle)
    if bg is None:
        return
    r.check("a background job survives the cancellation of its start request",
            job_state(m, vm, bg) in ("queued", "running"))
    ok, _ = m.tool("stop_job", name=vm, id=bg)
    r.check("stop_job still ends that background job", ok)


def cancellation_checks(m, vm, r, windows):
    """A foreground run_command cancelled over the real protocol (notifications/cancelled) is
    stopped in the VM, recoverable through list_jobs and get_job_status, and leaves unrelated
    work alone; a background start survives its own request's cancellation; a client that
    disconnects stops its foreground run (a second MCP session, so this one can observe)."""
    since = len(m.log)
    if windows:
        guest(m, vm, "Invoke-CimMethod Win32_Process -MethodName Create -Arguments "
                     "@{ CommandLine = 'ping.exe -n 8314 127.0.0.1' } | Out-Null")
        payload = ("[IO.File]::WriteAllText(\"$env:USERPROFILE\\cancel-marker\", 'x'); "
                   "Start-Process ping.exe -ArgumentList '-n 8311 127.0.0.1' -WindowStyle Hidden; Start-Sleep 900")
        marker = "Test-Path \"$env:USERPROFILE\\cancel-marker\""
        yes = "True"
        count = lambda n: ("@(Get-CimInstance Win32_Process | Where-Object "
                           f"{{ $_.CommandLine -like '*-n {n} *' }}).Count")
        mine, unrelated, mine_expected = count(8311), count(8314), 1
        bg_command = "ping.exe -n 8315 127.0.0.1"
        payload2 = ("[IO.File]::WriteAllText(\"$env:USERPROFILE\\disconnect-marker\", 'x'); "
                    "Start-Process ping.exe -ArgumentList '-n 8316 127.0.0.1' -WindowStyle Hidden; Start-Sleep 900")
        marker2 = "Test-Path \"$env:USERPROFILE\\disconnect-marker\""
        mine2 = count(8316)
    else:
        guest(m, vm, "sleep 8314", background=True)
        payload = ("printf x > ~/cancel-marker; sleep 8311 & sudo -n sleep 8312 & "
                   "env -i setsid sleep 8313 & wait")
        marker = "test -f ~/cancel-marker && echo yes || echo no"
        yes = "yes"
        mine, unrelated, mine_expected = "pgrep -fx 'sleep 831[123]' | wc -l", "pgrep -fx 'sleep 8314' | wc -l", 3
        bg_command = "sleep 8315"
        payload2 = "printf x > ~/disconnect-marker; sleep 8316 & wait"
        marker2 = "test -f ~/disconnect-marker && echo yes || echo no"
        mine2 = "pgrep -fx 'sleep 8316' | wc -l"

    rid, fut = m.start_call("run_command", name=vm, command=payload, timeout=120)
    try:
        r.check("a foreground payload is running before it is cancelled",
                poll(lambda: guest_out(m, vm, marker) == yes, 40))
        # The id a cancelling client never receives: found again by listing.
        running = [j for j in (list_all_jobs(m, vm) or [])
                   if j.get("kind") == "foreground" and j.get("state") == "running"]
        r.check("list_jobs recovers the in-flight run: exactly one running foreground job",
                len(running) == 1, f"{len(running)} running foreground jobs")
        job = running[0]["job_id"] if len(running) == 1 else None
        spawned = processes_present(m, vm, r, "the foreground payload's own processes", mine, mine_expected)
        m.cancel(rid)
        if job is not None:
            def stopped():
                d = structured(m.call("get_job_status", name=vm, id=job))
                return d.get("state") == "stopped" and d.get("stop_reason") == "request_cancelled"
            r.check("notifications/cancelled stops the run: get_job_status says stopped, request_cancelled",
                    poll(stopped, 40, 1))
            listed = [j for j in (list_all_jobs(m, vm) or []) if j.get("job_id") == job]
            r.check("list_jobs reports the same stop for that job",
                    len(listed) == 1 and listed[0].get("state") == "stopped"
                    and listed[0].get("stop_reason") == "request_cancelled", f"{len(listed)} entries")
        processes_gone(m, vm, r, "the cancelled run's own processes", mine, spawned)
        r.check("a process the cancelled run did not start is still running", guest_out(m, vm, unrelated) == "1")
    finally:
        m.cancel(rid)  # harmless if already cancelled; ends the run if the checks above failed early
        m.forget(rid)

    survive_cancel_checks(m, vm, r, bg_command)

    # A client that disconnects: its foreground run is stopped, the VM and unrelated work stay.
    other = Mcp(m.binary, env={"AGENTPC_KEEP_RUNNING": "1"})
    try:
        rid2, _ = other.start_call("run_command", name=vm, command=payload2, timeout=120)
        r.check("a second session's foreground payload is running before it disconnects",
                poll(lambda: guest_out(m, vm, marker2) == yes, 40))
        spawned2 = processes_present(m, vm, r, "the second session's payload processes", mine2, 1)
        other.forget(rid2)
    finally:
        began = time.monotonic()
        other.close()
        closed = time.monotonic() - began
    print(f"  METRIC  disconnected session closed in {closed:.2f}s")
    processes_gone(m, vm, r, "the disconnected session's payload processes", mine2, spawned2)
    r.check("a process the disconnected run did not start is still running", guest_out(m, vm, unrelated) == "1")
    ok, out = m.tool("list_vms")
    try:
        state = next(v["state"] for v in json.loads(out[out.index("{"):]).get("instances", []) if v["name"] == vm)
    except (ValueError, StopIteration):
        state = None
    r.check("the VM keeps running after the other session disconnected", state == "running", str(state))
    guest(m, vm, "Get-CimInstance Win32_Process | Where-Object { $_.CommandLine -like '*-n 8314 *' } | "
                 "ForEach-Object { Stop-Process -Id $_.ProcessId -Force }" if windows else "pkill -fx 'sleep 8314'; true")
    metric("cancellation workflow (cancel, recover, background, disconnect)", m, since)


def listing_checks(m, vm, r, windows):
    """list_jobs over the real protocol: pagination, metadata only, a damaged record, a file named
    like the listing's error marker, bad arguments."""
    since = len(m.log)
    synthetic = "SYNTHETIC-NO-CREDENTIAL-CONTENT"
    fg = guest(m, vm, "'completed'" if windows else "printf 'completed\\n'").get("execution_id")
    bg = guest(m, vm, f"'{synthetic}'; exit 7" if windows else f"printf '{synthetic}\\n'; exit 7",
               background=True).get("job_id")
    live = guest(m, vm, "Start-Sleep 900" if windows else "sleep 900", background=True).get("job_id")
    for _ in range(3):
        guest(m, vm, "'tiny'" if windows else "printf tiny")
    poll(lambda: job_state(m, vm, bg) == "exited", 40, 0.5)

    res = m.call("list_jobs", name=vm, limit=2)
    d = structured(res)
    r.check("list_jobs: a bounded typed page with a continuation",
            not res.get("isError") and len(d.get("jobs", [])) == 2 and d.get("has_more") is True
            and bool(d.get("next_cursor")) and len(res["content"]) == 1, brief(res))
    rows, cursor = list(d.get("jobs", [])), d.get("next_cursor")
    newer = guest(m, vm, "'new-after-page'" if windows else "printf new-after-page").get("execution_id")
    clean = True
    for _ in range(60):
        if not cursor:
            break
        res = m.call("list_jobs", name=vm, limit=2, cursor=cursor)
        d = structured(res)
        clean = clean and not res.get("isError")
        rows += d.get("jobs", [])
        cursor = d.get("next_cursor") if d.get("has_more") else None
    ids = [j["job_id"] for j in rows]
    r.check("following next_cursor never fails, lists newest first without repeats, and a job created meanwhile does not shift it",
            clean and ids == sorted(set(ids), reverse=True) and newer not in ids, f"{len(ids)} jobs")
    byid = {j["job_id"]: j for j in rows}
    r.check("a foreground run, an exited background job and a live one are all listed",
            fg in byid and bg in byid and live in byid)
    entry = byid.get(bg, {})
    r.check("the exited background job shows its code and a log without any of its output",
            entry.get("exit_code") == 7 and entry.get("kind") == "background" and entry.get("log_available") is True
            and synthetic not in json.dumps(rows))
    r.check("the live background job is queued or running", byid.get(live, {}).get("state") in ("queued", "running"))

    # A damaged exit record degrades that one entry; a file named like the error marker is harmless.
    guest(m, vm, f"[IO.File]::WriteAllText(\"$env:USERPROFILE\\agentpc-bg\\{bg}.exit\", '1-2')" if windows
          else f"printf '1-2' > ~/agentpc-bg/{bg}.exit")
    guest(m, vm, "[IO.File]::WriteAllText(\"$env:USERPROFILE\\agentpc-bg\\AGENTPC_LSERR x\", 'x')" if windows
          else "touch ~/agentpc-bg/'AGENTPC_LSERR x'")
    res = m.call("list_jobs", name=vm, limit=MAX_LIST_LIMIT)
    damaged = {j["job_id"]: j for j in structured(res).get("jobs", [])}
    r.check("a damaged exit record makes one entry unknown, without hiding older jobs or failing the page",
            not res.get("isError") and damaged.get(bg, {}).get("state") == "unknown"
            and damaged.get(bg, {}).get("exit_code") is None and bool(damaged.get(bg, {}).get("note"))
            and fg in damaged and live in damaged, brief(res))

    for label, code, args in (("limit 0", "invalid_limit", {"limit": 0}),
                              (f"limit {MAX_LIST_LIMIT + 1}", "invalid_limit", {"limit": MAX_LIST_LIMIT + 1}),
                              ("a malformed cursor", "invalid_cursor", {"cursor": "bad-token"})):
        res = m.call("list_jobs", name=vm, **args)
        r.check(f"list_jobs with {label}: structured error {code}",
                res.get("isError") is True and structured(res).get("error_code") == code, brief(res))
    r.check("listing left the live job running", job_state(m, vm, live) in ("queued", "running"))
    m.call("stop_job", name=vm, id=live)
    metric("list_jobs workflow (pages, damaged record, bad arguments)", m, since)


def stale_cursor_before(m, vm):
    """A page cursor to take across a checkpoint restore (None if the VM has too few jobs)."""
    res = m.call("list_jobs", name=vm, limit=1)
    d = structured(res)
    return d.get("next_cursor") if not res.get("isError") and d.get("has_more") else None


def stale_cursor_after(m, vm, r, cursor):
    """After restore_vm the VM's generation changed: the old cursor is refused, never restarted."""
    if cursor is None:
        r.check("a page cursor was available before the restore", False, "list_jobs returned no next_cursor")
        return
    res = m.call("list_jobs", name=vm, cursor=cursor)
    d = structured(res)
    r.check("a list_jobs cursor from before restore_vm is refused as cursor_stale, with no jobs",
            res.get("isError") is True and d.get("error_code") == "cursor_stale" and d.get("jobs") == [], brief(res))
    r.check("the stale-cursor message does not claim the jobs are gone",
            "are gone" not in (d.get("error") or "") and "without cursor" in (d.get("error") or ""), brief(res))
    res = m.call("list_jobs", name=vm, limit=2)
    r.check("list_jobs without a cursor works again after the restore", not res.get("isError"), brief(res))


SSH_PORT = 22  # the guest's own sshd listens on its loopback on every OS: a TCP target needing no fixture


def readiness_wait(m, vm, **kw):
    res = m.call("wait_for_service", name=vm, **kw)
    return res, structured(res)


def readiness_fixture_available(m, vm, windows):
    """The interpreter the HTTP fixture runs under, or None: "powershell" on Windows; in a Linux
    guest python3 where it has it, else perl with IO::Socket::INET (as the forward_port check does).
    Nothing is installed or downloaded to make one available."""
    if windows:
        return "powershell"
    found = guest_out(m, vm, "if command -v python3 >/dev/null 2>&1; then echo python3; "
                             "elif perl -MIO::Socket::INET -e 1 >/dev/null 2>&1; then echo perl; else echo none; fi")
    return found if found in ("python3", "perl") else None


def readiness_tcp_checks(m, vm, r, windows):
    """TCP readiness against the guest's own sshd, which needs no fixture, plus the closed-port and
    job shortcuts."""
    sleeper = guest(m, vm, "Start-Sleep 900" if windows else "sleep 900", background=True).get("job_id")
    r.check("a background job to wait alongside started", sleeper is not None)
    res, d = readiness_wait(m, vm, guest_port=SSH_PORT, job_id=sleeper, timeout_seconds=30)
    r.check("TCP readiness: the guest's own sshd on 127.0.0.1 is accepted, with the job observed alongside",
            not res.get("isError") and d.get("state") == "ready" and d.get("tcp") == "accepted"
            and d.get("criterion_met") is True and len(res["content"]) == 1, brief(res))
    res, d = readiness_wait(m, vm, guest_port=CLOSED_PORT, timeout_seconds=3)
    r.check("a port nothing listens on times out honestly, never a guess",
            res.get("isError") is True and d.get("state") == "timed_out" and d.get("tcp") in ("refused", "timed_out"),
            brief(res))
    r.check("that timeout did not stop the job", job_state(m, vm, sleeper) in ("queued", "running"))

    failing = guest(m, vm, "exit 7", background=True).get("job_id")
    poll(lambda: job_state(m, vm, failing) == "exited", 40, 0.5)
    began = time.monotonic()
    res, d = readiness_wait(m, vm, guest_port=CLOSED_PORT, job_id=failing, timeout_seconds=30)
    r.check("a job proven failed ends the wait early as job_failed",
            res.get("isError") is True and d.get("state") == "job_failed" and time.monotonic() - began < 20, brief(res))
    done = guest(m, vm, "exit 0").get("execution_id")
    res, d = readiness_wait(m, vm, guest_port=SSH_PORT, job_id=done, timeout_seconds=10)
    r.check("a job that exited 0 is not failure: the listener is still ready",
            not res.get("isError") and d.get("state") == "ready", brief(res))
    res, d = readiness_wait(m, vm, guest_port=SSH_PORT, job_id=999999999, timeout_seconds=10)
    r.check("a job id that was never recorded is not failure either",
            not res.get("isError") and d.get("state") == "ready", brief(res))
    if sleeper is not None:
        ok, _ = m.tool("stop_job", name=vm, id=sleeper)
        r.check("stop_job ends the waited-alongside job", ok)


def readiness_http_checks(m, vm, r, windows, kind):
    """A late TCP listener, HTTP statuses, an unfollowed redirect and a cancelled probe, against
    fixtures started in the guest (see readiness_fixture_available)."""
    wait = lambda **kw: readiness_wait(m, vm, **kw)

    # Late readiness: a TCP listener that starts after the wait began.
    late_port = READY_PORT + 2
    late = (f"Start-Sleep 4; $l = [Net.Sockets.TcpListener]::new([Net.IPAddress]::Loopback, {late_port}); $l.Start(); Start-Sleep 600"
            if windows else late_listener_command(kind, late_port))
    late_job = guest(m, vm, late, background=True).get("job_id")
    began = time.monotonic()
    res, d = wait(guest_port=late_port, timeout_seconds=40)
    took = time.monotonic() - began
    r.check("a TCP listener that starts late is waited for: ready, accepted, after more than one attempt",
            not res.get("isError") and d.get("state") == "ready" and d.get("tcp") == "accepted"
            and d.get("criterion_met") is True and (d.get("attempts") or 0) >= 2 and took >= 3, f"{took:.1f}s {brief(res)}")
    if late_job is not None:
        m.call("stop_job", name=vm, id=late_job)

    server = guest(m, vm, WINDOWS_HTTP_SERVER if windows else linux_http_server(kind), background=True).get("job_id")
    try:
        res, d = wait(guest_port=READY_PORT, protocol="http", job_id=server, timeout_seconds=40)
        r.check("HTTP readiness: the expected status is observed; no response body, header or guest text is returned",
                not res.get("isError") and d.get("state") == "ready" and d.get("http_status") == 200
                and d.get("http_outcome") == "status_matched" and d.get("criterion_met") is True
                and "SYNTHETIC-RESPONSE-BODY" not in json.dumps(res) and len(res["content"]) == 1, brief(res))
        res, d = wait(guest_port=READY_PORT, protocol="http", path="/mismatch", expected_status=503, timeout_seconds=10)
        r.check("an explicit alternate expected status is met", not res.get("isError") and d.get("http_status") == 503,
                brief(res))
        res, d = wait(guest_port=READY_PORT, protocol="http", path="/mismatch", timeout_seconds=3)
        r.check("a status mismatch times out keeping the last observation",
                res.get("isError") is True and d.get("state") == "timed_out" and d.get("timed_out") is True
                and d.get("http_outcome") == "status_mismatched" and d.get("http_status") == 503, brief(res))
        res, d = wait(guest_port=READY_PORT, protocol="http", path="/redirect", timeout_seconds=3)
        r.check("a redirect is observed as redirect, not followed",
                res.get("isError") is True and d.get("http_outcome") == "redirect" and d.get("http_status") == 302,
                brief(res))
        counted = guest_out(m, vm, "if (Test-Path \"$env:USERPROFILE\\ready-target-count\") "
                                   "{ [IO.File]::ReadAllText(\"$env:USERPROFILE\\ready-target-count\") } else { '0' }"
                            if windows else "if [ -f ~/ready-target-count ]; then cat ~/ready-target-count; else printf 0; fi")
        r.check("the redirect target was never requested", counted == "0", counted[:20])
        res, d = wait(guest_port=READY_PORT, protocol="http", path="/Unicode-é space", timeout_seconds=10)
        r.check("a Unicode path with a space is escaped and answered", not res.get("isError") and d.get("http_status") == 200,
                brief(res))
        r.check("the waits did not stop the serving job", job_state(m, vm, server) in ("queued", "running"))

        # Cancel a probe in flight (a slow reply), over the real protocol.
        rid, _ = m.start_call("wait_for_service", name=vm, guest_port=READY_PORT, protocol="http",
                              path="/slow", timeout_seconds=30)
        time.sleep(1)
        m.cancel(rid)
        m.forget(rid)
        res, d = wait(guest_port=READY_PORT, timeout_seconds=10)
        r.check("after a cancelled wait the listener still answers", not res.get("isError") and d.get("state") == "ready",
                brief(res))
        r.check("a cancelled wait left the serving job running", job_state(m, vm, server) in ("queued", "running"))
        res = m.call("list_forwards", name=vm)
        r.check("readiness created no forward", not res.get("isError") and str(READY_PORT) not in text_of(res), brief(res))
    finally:
        if server is not None:
            ok, _ = m.tool("stop_job", name=vm, id=server)
            r.check("stop_job ends the serving job", ok)


def readiness_checks(m, vm, r, windows):
    """wait_for_service against real guest listeners. The TCP checks need only the guest's sshd. The
    HTTP and late-listener checks need a fixture (python3 in a Linux guest); when it is missing that
    is a FAILED check, so a green run can never be mistaken for readiness coverage. Nothing is
    installed or downloaded to work around it."""
    since = len(m.log)
    readiness_tcp_checks(m, vm, r, windows)
    kind = readiness_fixture_available(m, vm, windows)
    r.check("readiness fixture available: HTTP and late-listener coverage needs python3 or perl in a Linux guest",
            kind is not None, "this guest has neither python3 nor perl, so the HTTP readiness checks did not run")
    if kind is not None:
        readiness_http_checks(m, vm, r, windows, kind)
    metric("readiness workflow (sshd TCP, job shortcuts, late listener, HTTP statuses, redirect, cancel)", m, since)


def recovery_checks(m, vm, r, windows):
    cancellation_checks(m, vm, r, windows)
    listing_checks(m, vm, r, windows)
    readiness_checks(m, vm, r, windows)


def inspect_checks(m, vm, r, windows):
    since = len(m.log)
    res = m.call("list_desktop_tools", name=vm, tool="get_desktop_state")
    try:
        marks = json.loads(text_of(res)).get("annotations") or {}
    except ValueError:
        marks = {}
    r.check("the driver's full schema for an observation tool carries readOnlyHint", marks.get("readOnlyHint") is True,
            brief(res))
    app = "notepad" if windows else "xfce4-terminal"
    res = m.call("list_desktop_tools", name=vm, tool="launch_app")
    try:
        marks = json.loads(text_of(res)).get("annotations") or {}
    except ValueError:
        marks = {"unreadable": True}
    r.check("the driver does not mark launch_app read-only", marks.get("readOnlyHint") is not True, brief(res))

    began = time.monotonic()
    res = m.call("inspect_desktop", name=vm, tool="get_desktop_state", arguments={})
    took = time.monotonic() - began
    r.check("inspect_desktop returns a real desktop observation (screenshot included)",
            not res.get("isError") and any(c.get("type") == "image" for c in res.get("content", [])), brief(res))
    metric(f"inspect_desktop get_desktop_state ({took:.2f}s round trip)", m, since)

    listing = ("(Get-Process notepad -ErrorAction SilentlyContinue | Select-Object -ExpandProperty Id "
               "| Sort-Object) -join ','" if windows else "pgrep -x xfce4-terminal | sort | tr '\\n' ','; true")

    def processes():
        return structured(m.call("run_command", name=vm, command=listing, timeout=30)).get("stdout", "").strip()

    before = processes()
    refused = m.call("inspect_desktop", name=vm, tool="launch_app", arguments={"name": app})
    r.check("inspect_desktop refuses a tool that acts (launch_app) and says why",
            refused.get("isError") is True and "read-only" in text_of(refused).lower(), brief(refused))
    refused = m.call("inspect_desktop", name=vm, tool="type_text", arguments={"text": "must not be typed"})
    r.check("inspect_desktop refuses type_text as read-only, not as a bad-argument error",
            refused.get("isError") is True and "read-only" in text_of(refused).lower(), brief(refused))
    unknown = m.call("inspect_desktop", name=vm, tool="no_such_inner_tool", arguments={})
    r.check("inspect_desktop refuses an unknown tool", unknown.get("isError") is True, brief(unknown))
    time.sleep(2)  # a launch that slipped through would have shown its process by now
    r.check("the refused calls changed nothing in the VM", processes() == before)

    res = m.call("use_desktop_tool", name=vm, tool="get_desktop_state", arguments={})
    r.check("use_desktop_tool still observes (the acting route is unchanged)",
            not res.get("isError") and any(c.get("type") == "image" for c in res.get("content", [])), brief(res))


def compact_observation(m, vm, r, pid, window_id, query=None):
    """get_window_state through inspect_desktop, with and without its screenshot; prints what
    each reply measured (they are measurements of two calls, not a comparison of versions)."""
    args = {"pid": pid, "window_id": window_id, "timeout_ms": 5000}
    full_since = len(m.log)
    full = m.call("inspect_desktop", name=vm, tool="get_window_state", arguments=dict(args))
    metric("inspect_desktop get_window_state, default (with screenshot)", m, full_since)
    compact = dict(args, include_screenshot=False)
    if query:
        compact["query"] = query
    since = len(m.log)
    res = m.call("inspect_desktop", name=vm, tool="get_window_state", arguments=compact)
    metric("inspect_desktop get_window_state, include_screenshot=false" + (" + query" if query else ""), m, since)
    r.check("inspect_desktop observes a window's UI tree without a screenshot (compact form)",
            not res.get("isError") and not any(c.get("type") == "image" for c in res.get("content", [])),
            brief(res))
    r.check("the default observation still carries the screenshot",
            not full.get("isError") and any(c.get("type") == "image" for c in full.get("content", [])),
            brief(full))


def ubuntu(m, vm, r):
    linux(m, vm, r, "google-chrome", "Chrome")
    observation(m, vm, r)
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
    observation(m, vm, r)
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
        compact_observation(m, vm, r, pid, win["window_id"], query="documentation")

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
    if job:
        sh(f"kill $(cat ~/agentpc-bg/{job.group(1)}.pid)")
        def ended():
            _, st = m.tool("get_job_status", name=vm, id=int(job.group(1)), tail_lines=1)
            return st if "ended without an exit code" in st else None
        r.check("a killed background job is reported ended", poll(ended, 20), out[:200])

    # stop_job stops the job and everything it started, root processes (sudo) included.
    ok, out = sh("sleep 3301 & sleep 3302 & sudo -n sleep 3303 & wait", background=True)
    job = re.search(r"\(id (\d+)", out)
    if r.check("background run_command points at stop_job", ok and job and "stop_job" in out, out[:300]):
        jid = int(job.group(1))
        count = "pgrep -fx 'sleep 330[123]' | wc -l"
        r.check("the job's processes are running", poll(lambda: stdout(sh(count)[1]) == "3", 15))
        ok, out = m.tool("stop_job", name=vm, id=jid)
        r.check("stop_job stops the job", ok and "stopped job" in out, out[:300])
        r.check("stop_job leaves none of its processes behind", stdout(sh(count)[1]) == "0")
        _, st = m.tool("get_job_status", name=vm, id=jid, tail_lines=1)
        r.check("get_job_status reports the job stopped", "STATE: stopped by stop_job" in st, st[:200])
        ok, out = m.tool("stop_job", name=vm, id=jid)
        r.check("stop_job again is harmless", ok, out[:200])
    ok, out = sh("echo quick", background=True)
    job = re.search(r"\(id (\d+)", out)
    if job:
        poll(lambda: "STATE: exited" in m.tool("get_job_status", name=vm, id=int(job.group(1)))[1], 15)
        ok, out = m.tool("stop_job", name=vm, id=int(job.group(1)))
        r.check("stop_job leaves a finished job as it was", ok and "already exited with code 0" in out, out[:200])
    ok, out = m.tool("stop_job", name=vm, id=1)
    r.check("stop_job refuses an unknown job", not ok and "no such job" in out, out[:200])
    # A job that started a server and returned: its wrapper exited, the server must still stop.
    ok, out = sh("nohup sleep 3304 >/dev/null 2>&1 & echo started", background=True)
    job = re.search(r"\(id (\d+)", out)
    if job:
        alive = "pgrep -fx 'sleep 3304' | wc -l"
        poll(lambda: "STATE: exited 0" in m.tool("get_job_status", name=vm, id=int(job.group(1)))[1], 15)
        r.check("a server the job left running is still up", stdout(sh(alive)[1]) == "1")
        ok, out = m.tool("stop_job", name=vm, id=int(job.group(1)))
        r.check("stop_job stops what an exited job left running", ok and "left running" in out
                and stdout(sh(alive)[1]) == "0", out[:200])
    # Processes that clear their environment and detach (one as root) stay in the job's scope.
    ok, out = sh("env -i setsid nohup sleep 3306 >/dev/null 2>&1 < /dev/null & "
                 "sudo -n env -i setsid nohup sleep 3307 >/dev/null 2>&1 < /dev/null & echo started",
                 background=True)
    job = re.search(r"\(id (\d+)", out)
    if job:
        esc = "pgrep -fx 'sleep 330[67]' | wc -l"
        r.check("detached env-cleared processes outlive their job",
                poll(lambda: stdout(sh(esc)[1]) == "2", 15))
        ok, out = m.tool("stop_job", name=vm, id=int(job.group(1)))
        r.check("stop_job stops detached env-cleared processes", ok and stdout(sh(esc)[1]) == "0",
                out[:200])

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
    page_cursor = stale_cursor_before(m, vm)
    ok, out = m.tool("checkpoint_vm", name=vm, label="smoke")
    if r.check("checkpoint_vm", ok, out[:200]):
        sh("echo after > ~/smoke-marker")
        ok, out = m.tool("restore_vm", name=vm, label="smoke")
        stale_cursor_after(m, vm, r, page_cursor)
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

    observation(m, vm, r)

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

    # stop_job ends the job's whole process tree, a program it started included.
    ok, out = ps("Start-Process ping -ArgumentList '-n 3300 127.0.0.1'; Start-Sleep 3300", background=True)
    job = re.search(r"\(id (\d+)", out)
    if job:
        jid = int(job.group(1))
        pings = "@(Get-Process ping -ErrorAction SilentlyContinue).Count"
        r.check("windows: the job's processes are running", poll(lambda: stdout(ps(pings)[1]) == "1", 30))
        ok, out = m.tool("stop_job", name=vm, id=jid)
        r.check("windows: stop_job stops the job", ok and "stopped job" in out, out[:300])
        r.check("windows: stop_job leaves no child behind", stdout(ps(pings)[1]) == "0")
        _, st = m.tool("get_job_status", name=vm, id=jid, tail_lines=1)
        r.check("windows: get_job_status reports the job stopped", "STATE: stopped by stop_job" in st, st[:200])

    # A job that starts a server and exits: stop_job still stops the server, and leaves alone a
    # process the job didn't start (created through WMI, so the SSH session's end keeps it).
    def tagged(n):
        return f"@(Get-CimInstance Win32_Process | Where-Object {{ $_.CommandLine -like '*-n {n} *' }}).Count"
    ps("Invoke-CimMethod Win32_Process -MethodName Create "
       "-Arguments @{ CommandLine = 'ping.exe -n 3302 127.0.0.1' } | Out-Null")
    ok, out = ps("Start-Process ping -ArgumentList '-n 3301 127.0.0.1' -WindowStyle Hidden; 'started'",
                 background=True)
    job = re.search(r"\(id (\d+)", out)
    if job:
        jid = int(job.group(1))
        r.check("windows: a server outlives its exited job", poll(
            lambda: "STATE: exited 0" in m.tool("get_job_status", name=vm, id=jid)[1]
            and stdout(ps(tagged(3301))[1]) == "1", 60))
        ok, out = m.tool("stop_job", name=vm, id=jid)
        r.check("windows: stop_job stops what an exited job left running",
                ok and "left running" in out and stdout(ps(tagged(3301))[1]) == "0", out[:300])
        r.check("windows: stop_job leaves an unrelated process alone", stdout(ps(tagged(3302))[1]) == "1")
        ps("Get-CimInstance Win32_Process | Where-Object { $_.CommandLine -like '*-n 3302 *' } | "
           "ForEach-Object { Stop-Process -Id $_.ProcessId -Force }")

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
    page_cursor = stale_cursor_before(m, vm)
    ok, out = m.tool("checkpoint_vm", name=vm, label="smoke")
    if r.check("windows: checkpoint_vm", ok, out[:200]):
        ps("Set-Content $HOME\\smoke-marker after")
        ok, out = m.tool("restore_vm", name=vm, label="smoke")
        stale_cursor_after(m, vm, r, page_cursor)
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
    observation(m, vm, r, windows=True)
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
    compact_observation(m, vm, r, pid, main["window_id"])

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

    refuse_unrelated_orphans(a.bin)
    reap_stale_vms(a.bin)
    failed = 0
    proto = Report()
    print("protocol: tool list, schemas, annotations and errors (no VM)")
    try:
        protocol_checks(a.bin, proto)
    except Exception as e:  # report and carry on to the VMs
        proto.check("no unexpected error", False, repr(e)[:300])
    failed += proto.failed
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
