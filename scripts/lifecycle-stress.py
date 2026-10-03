#!/usr/bin/env python3
"""Stress MCP lifecycle races using task-owned Ubuntu VMs from an existing snapshot.

Usage: python3 scripts/lifecycle-stress.py --bin target/release/agentpc --rounds 3
No image downloads/builds. Refuses to start if unrelated running orphan VMs exist.
Protocol replies stay in memory; logs contain only check labels, never viewer credentials.
"""

import argparse
from concurrent.futures import Future, ThreadPoolExecutor
import json
import os
from pathlib import Path
import subprocess
import threading
import time
import uuid


class Mcp:
    def __init__(self, binary):
        env = os.environ.copy()
        env.pop("AGENTPC_KEEP_RUNNING", None)
        self.proc = subprocess.Popen([binary, "mcp"], stdin=subprocess.PIPE,
                                     stdout=subprocess.PIPE, stderr=subprocess.DEVNULL, env=env)
        self.lock = threading.Lock()
        self.pending = {}
        self.next_id = 0
        threading.Thread(target=self.read, daemon=True).start()
        self.request("initialize", {"protocolVersion": "2025-06-18", "capabilities": {},
                     "clientInfo": {"name": "agentpc-lifecycle-stress", "version": "1"}})
        self.send({"jsonrpc": "2.0", "method": "notifications/initialized"})

    def send(self, message):
        with self.lock:
            self.proc.stdin.write((json.dumps(message) + "\n").encode())
            self.proc.stdin.flush()

    def read(self):
        try:
            for line in self.proc.stdout:
                reply = json.loads(line)
                with self.lock:
                    future = self.pending.pop(reply.get("id"), None)
                if future:
                    if "error" in reply:
                        future.set_exception(RuntimeError("MCP protocol error"))
                    else:
                        future.set_result(reply.get("result", {}))
        finally:
            with self.lock:
                pending, self.pending = self.pending, {}
            for future in pending.values():
                future.set_exception(RuntimeError("MCP server exited"))

    def request(self, method, params):
        future = Future()
        with self.lock:
            self.next_id += 1
            request_id = self.next_id
            self.pending[request_id] = future
        self.send({"jsonrpc": "2.0", "id": request_id, "method": method, "params": params})
        return future.result(timeout=180)

    def tool(self, name, /, **arguments):
        result = self.request("tools/call", {"name": name, "arguments": arguments})
        text = "\n".join(c.get("text", "") for c in result.get("content", []) if c.get("type") == "text")
        return not result.get("isError", False), text

    def close(self, kill=False):
        if self.proc.poll() is not None:
            return
        if kill:
            self.proc.kill()
        else:
            self.proc.stdin.close()
        try:
            self.proc.wait(timeout=60)
        except subprocess.TimeoutExpired:
            self.proc.kill()
            self.proc.wait(timeout=10)
            raise RuntimeError("MCP shutdown timed out") from None


def inventory(binary):
    result = subprocess.run([binary, "list", "--json"], capture_output=True, timeout=30)
    if result.returncode:
        raise RuntimeError("could not inventory VMs")
    return json.loads(result.stdout)


def state(binary, name):
    return next((v["state"] for v in inventory(binary)["instances"] if v["name"] == name), None)


def require(label, passed):
    print(f"{'PASS' if passed else 'FAIL'} {label}", flush=True)
    if not passed:
        raise RuntimeError(label)


def parallel(client, calls):
    with ThreadPoolExecutor(max_workers=len(calls)) as pool:
        return list(pool.map(lambda c: client.tool(c[0], **c[1]), calls))


def run(binary, rounds):
    before = inventory(binary)
    image = next((i for i in before["images"] if i["image"] == "ubuntu-24.04"), None)
    require("existing, current Ubuntu snapshot available (no build/download)",
            image and image["fast_start"] and not image.get("outdated"))
    require("no unrelated running orphan VMs", not any(
        v["state"] != "stopped" and v.get("owner_running") is False for v in before["instances"]))
    prefix = "stress-" + uuid.uuid4().hex[:12]
    names, clients = [], []
    client = Mcp(binary)
    clients.append(client)
    try:
        for n in range(rounds):
            vm = f"{prefix}-{n}"
            names.append(vm)
            results = parallel(client, [("create_vm", {"os": "ubuntu", "name": vm})] * 2)
            require(f"round {n}: concurrent named creates produce a usable VM", any(ok for ok, _ in results))
            require(f"round {n}: named create retry succeeds", client.tool("create_vm", os="ubuntu", name=vm)[0])
            require(f"round {n}: only one named VM exists", sum(
                v["name"] == vm for v in inventory(binary)["instances"]) == 1)
            require(f"round {n}: mismatched retry settings refused", not client.tool(
                "create_vm", os="ubuntu", name=vm, cpus=1)[0])
            require(f"round {n}: concurrent starts succeed", all(ok for ok, _ in parallel(
                client, [("start_vm", {"name": vm})] * 3)))
            require(f"round {n}: concurrent stops succeed", all(ok for ok, _ in parallel(
                client, [("stop_vm", {"name": vm})] * 3)))
            require(f"round {n}: stopped state is consistent", state(binary, vm) == "stopped")
            require(f"round {n}: mixed start/stop race completes", all(ok for ok, _ in parallel(
                client, [("start_vm", {"name": vm}), ("stop_vm", {"name": vm})])))
            require(f"round {n}: recover after mixed race", client.tool("start_vm", name=vm)[0])
            require(f"round {n}: shell responds after races", client.tool(
                "run_command", name=vm, command="true", timeout=15)[0])
            if n < rounds - 1:
                parallel(client, [("start_vm", {"name": vm}), ("delete_vm", {"name": vm})])
                require(f"round {n}: start/delete race leaves VM deleted", state(binary, vm) is None)

        live = names[-1]
        orphan = f"{prefix}-orphan"
        names.append(orphan)
        victim = Mcp(binary)
        clients.append(victim)
        require("second session creates its own VM", victim.tool("create_vm", os="ubuntu", name=orphan)[0])
        victim.close(kill=True)
        require("killed MCP leaves its VM running", state(binary, orphan) == "running")
        recovered = Mcp(binary)
        clients.append(recovered)
        deadline = time.monotonic() + 60
        while state(binary, orphan) != "stopped" and time.monotonic() < deadline:
            time.sleep(1)
        require("new MCP server reaps killed session's VM", state(binary, orphan) == "stopped")
        require("orphan cleanup leaves live session VM running", state(binary, live) == "running")
        require("restarted session adopts existing named VM", recovered.tool(
            "create_vm", os="ubuntu", name=orphan)[0])
        recovered.close()
        require("normal session EOF stops its adopted VM", state(binary, orphan) == "stopped")
        require("normal session EOF leaves other owner's VM running", state(binary, live) == "running")
        client.close()
        require("normal session EOF stops its own VM", state(binary, live) == "stopped")
        # Existing VM configurations must not change during the test.
        current = {v["name"]: v for v in inventory(binary)["instances"]}
        fields = ("state", "image", "owner", "slot", "memory_gb", "cpus", "offline")
        require("pre-existing VMs unchanged", all(v["name"] in current and all(
            v.get(f) == current[v["name"]].get(f) for f in fields) for v in before["instances"]))
    finally:
        for c in reversed(clients):
            try:
                c.close()
            except Exception:
                print("WARN task-owned MCP cleanup failed", flush=True)
        # Only exact unique names registered by this invocation are eligible for cleanup.
        for name in names:
            if state(binary, name) is not None:
                result = subprocess.run([binary, "rm", name], stdout=subprocess.DEVNULL,
                                        stderr=subprocess.DEVNULL, timeout=90)
                if result.returncode:
                    print(f"FAIL cleanup {name}", flush=True)
        require("all task-owned VMs deleted", all(state(binary, name) is None for name in names))
    print("STRESS DONE", flush=True)


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--bin", default="target/release/agentpc")
    parser.add_argument("--rounds", type=int, default=3)
    args = parser.parse_args()
    if not 1 <= args.rounds <= 20:
        parser.error("--rounds must be 1–20")
    try:
        run(str(Path(args.bin).resolve()), args.rounds)
    except Exception as error:
        # Never print protocol/tool responses: they can contain viewer credentials.
        print(f"STRESS FAILED: {type(error).__name__}", flush=True)
        raise SystemExit(1) from None
