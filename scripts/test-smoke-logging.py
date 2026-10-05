#!/usr/bin/env python3
"""Regression: smoke.py must never print a viewer URL or the credential in one.

A lifecycle reply (create_vm, reset_vm, restore_vm, start_vm, list_vms) carries the viewer
link, whose fragment holds the VM's VNC password. The suite once printed such a reply when a
check failed. These tests use a synthetic placeholder, never a real credential, and run no
VM and no MCP server.
"""

import contextlib
import importlib.util
import io
import json
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
spec = importlib.util.spec_from_file_location("smoke", ROOT / "scripts" / "smoke.py")
smoke = importlib.util.module_from_spec(spec)
spec.loader.exec_module(smoke)

PLACEHOLDER = "SYNTHETIC-PLACEHOLDER-NOT-A-REAL-SECRET"
VIEWER = (
    "http://127.0.0.1:8100/vnc.html#autoconnect=1&resize=scale&host=127.0.0.1&port=47200"
    f"&path=&password={PLACEHOLDER}"
)
LIFECYCLE_REPLY = "reset vm-1; viewer " + VIEWER + "\nready"
LIST_REPLY = json.dumps({"instances": [{"name": "vm-1", "state": "running", "viewer": VIEWER}]})
errors = []


def require(condition, message):
    if not condition:
        errors.append(message)


def clean(text, label):
    require(PLACEHOLDER not in text, f"{label}: the credential was printed")
    require("vnc.html" not in text and "://" not in text and "autoconnect" not in text,
            f"{label}: viewer link was printed")


def printed(fn):
    buffer = io.StringIO()
    with contextlib.redirect_stdout(buffer):
        fn()
    return buffer.getvalue()


def client(replies):
    """A Mcp with no server behind it: request() answers tools/call with replies[tool]."""
    m = smoke.Mcp.__new__(smoke.Mcp)
    m.calls, m.log = 0, []
    m.request = lambda method, params: {
        "content": [{"type": "text", "text": replies[params["name"]]}], "isError": False,
    }
    return m


# 1. Report.check: the exact failing shape (a lifecycle reply sliced into the detail), at
#    every slice the suite uses, plus the credential cut out of its URL.
r = smoke.Report()
for cut in (100, 150, 200, 300, len(LIFECYCLE_REPLY)):
    out = printed(lambda: r.check("reset_vm returns to a clean install", False,
                                  f"{LIFECYCLE_REPLY[:cut]} | left"))
    clean(out, f"Report.check slice {cut}")
    require("FAIL" in out and "reset_vm returns to a clean install" in out, "the failure and its label are still printed")
out = printed(lambda: r.check("tail slice", False, LIFECYCLE_REPLY[-80:]))
clean(out, "Report.check tail slice")
out = printed(lambda: r.check("quoted pair", False, f'{{"password": "{PLACEHOLDER}"}} password: {PLACEHOLDER}'))
clean(out, "Report.check key/value forms")
require(r.failed == 7, "failed checks are still counted")
out = printed(lambda: r.check("passing", True, VIEWER))
require("PASS" in out and PLACEHOLDER not in out, "a passing check prints no detail")

# 2. Source: lifecycle tools come back redacted from Mcp.tool(), so nothing derived from the
#    reply (slices, repr, list_vms parsing) can carry the credential; other tools are untouched.
m = client({tool: LIFECYCLE_REPLY for tool in ("create_vm", "start_vm", "reset_vm", "restore_vm", "checkpoint_vm")})
for tool in ("create_vm", "start_vm", "reset_vm", "restore_vm", "checkpoint_vm"):
    ok, text = m.tool(tool, name="vm-1")
    clean(text, f"Mcp.tool({tool})")
    require("ready" in text and ok, f"Mcp.tool({tool}) keeps the reply's other text and its status")
m = client({"list_vms": LIST_REPLY})
ok, text = m.tool("list_vms")
clean(text, "Mcp.tool(list_vms)")
require([v["name"] for v in json.loads(text)["instances"]] == ["vm-1"], "list_vms stays valid JSON with its names")
m = client({"run_command": "exit code: 0\n--- stdout ---\nhttp://example.com/#a=1&password=guest-output"})
require("guest-output" in m.tool("run_command", name="vm-1", command="x")[1],
        "ordinary command output is not rewritten")

# 3. Exceptions and tracebacks.
try:
    raise RuntimeError(f"tools/call: {LIFECYCLE_REPLY}")
except RuntimeError as e:
    require(PLACEHOLDER not in smoke.redact(repr(e)), "repr of an exception is redacted by redact()")
    shown = io.StringIO()
    with contextlib.redirect_stderr(shown):
        smoke.safe_excepthook(*sys.exc_info())
    clean(shown.getvalue(), "excepthook")
    require("RuntimeError" in shown.getvalue(), "the traceback still names the error")
require(sys.excepthook is smoke.safe_excepthook, "smoke.py installs the redacting excepthook")

# 4. The safe labels the new checks print stay informative.
res = {"isError": True, "content": [{"type": "text", "text": LIFECYCLE_REPLY}],
       "structuredContent": {"state": "exited", "exit_code": 3, "stdout": VIEWER}}
shown = smoke.brief(res)
clean(shown, "brief()")
require("exit_code" in shown and "'state': 'exited'" in shown, "brief() keeps typed labels")

if errors:
    for error in errors:
        print(f"FAIL: {error}")
    sys.exit(1)
print("PASS: smoke.py prints no viewer link or credential (checks, tool replies, tracebacks)")
