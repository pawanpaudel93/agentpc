# Execution and Observation Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development or superpowers:executing-plans to implement this plan task-by-task. No commit steps are authorized.

**Goal:** Give agents reliable, structured execution outcomes, efficient job observation, and a fail-closed read-only desktop route.

**Architecture:** Keep transport execution separate from user payload tracking. Introduce typed result contracts and platform-specific execution tracking as needed without replacing the lifecycle backend. Extend existing job observation and reuse the existing driver gateway.

**Tech Stack:** Rust; rmcp 3.4.1; Tokio 1.53.1; serde/schemars; guest bash/systemd and Windows PowerShell 5.1; Python smoke tests.

## Global Constraints

- Follow docs/superpowers/specs/2026-10-04-execution-observation-design.md, approved 2026-10-04.
- Work on feat/execution-observation, not main. Native worktree creation failed; existing checkout is the permitted fallback.
- No commits, pushes, releases, new image builds, or changes to existing research notes.
- Existing foreground commands remain foreground; separate stdout/stderr, exit codes and partial output stay available.
- Never signal a bare recorded PID or session without identity proof. Cleanup is bounded; uncertainty is reported, never hidden.
- get_job_status wait_seconds defaults to 0 and has maximum 30. Cursor is optional; no-cursor tail behavior stays compatible. Waiting never cancels a job.
- inspect_desktop dispatches only tools explicitly marked read-only. Unknown metadata fails closed. Reconnection invalidates cached metadata.
- Preserve human-readable text and add structured results/fallback. Nonzero command exits remain tool errors.
- No invented driver parameters or client-independent notification promises.
- Never read/print/store credentials or .env files. Test logs must not print viewer URLs.
- Long jobs/builds use a Monitor. Real tests use uniquely named task-owned VMs and delete them in finally cleanup.

### Task 1: Typed command results and tracked foreground cancellation

**Files:** src/mcp.rs; optionally src/execution.rs and src/main.rs for a module declaration. Unit tests alongside the implementation.

**Interfaces:** Keep low-level infrastructure exec(name, command, timeout, desktop_env) behavior available to internal callers. User run_command returns CallToolResult with structured_content and readable content. Define serializable/JsonSchema command outcomes with fields: schema_version, execution_id/job_id, state, exit_code, stdout, stderr, timed_out, output_truncated, termination_confirmed, effect_unknown, recovery_action (nullable where inapplicable). Success/error schemas must match actual data; metadata is typed, not extracted from human text. Expose the ID to get_job_status/stop_job when execution remains uncertain.

- [ ] Write failing tests for serialization and tool result semantics. Representative assertions:

```rust
assert_eq!(result.structured_content.as_ref().unwrap()["timed_out"], true);
assert_eq!(result.is_error, Some(true));
assert!(result.content.iter().any(|c| c.as_text().is_some()));
assert_eq!(result.structured_content.as_ref().unwrap()["termination_confirmed"], false);
```

- [ ] Add platform-specific foreground tracking before payload start. Linux should reuse verified session/cgroup identity where possible; Windows should record PID plus creation-time/boot proof before the payload. Metadata must not pollute payload stdout/stderr.
- [ ] Attempt guest-side cancellation on timeout under a bounded deadline, preserving partial output. Keep local pipe drains bounded if remote children retain pipes. Connection-loss/startup uncertainty is not normal completion; never retry payload code automatically.
- [ ] Return typed success, nonzero exit, timeout-confirmed, timeout-unconfirmed, and startup/transport failures with accurate legacy text.
- [ ] Add actual process-identity/termination unit or fixture tests where host platform permits; do not weaken identity checks merely to make a simulated test pass.
- [ ] Run cargo fmt, cargo test and cargo clippy --locked --all-targets -- -D warnings. Do not commit.

### Task 2: Structured job state, bounded waits, and incremental output

**Files:** src/mcp.rs and the execution module from Task 1 if present. Tests alongside implementation.

**Interfaces:** JobArgs adds wait_seconds: u64 (default 0, bounded 30) and cursor: Option<String>, retains tail_lines. get_job_status returns typed state, exit_code, log output, output_truncated, next_cursor, cursor_reset, wait_timed_out. Existing default text STATE/log-tail format remains readable. Existing execution IDs from Task 1 can be queried/stopped when applicable.

- [ ] Add failing tests for zero-wait compatibility and invalid wait bounds:

```rust
let args: JobArgs = serde_json::from_str(r#"{"name":"vm","id":7}"#).unwrap();
assert_eq!(args.wait_seconds, 0);
assert!(args.cursor.is_none());
```

- [ ] Return structured guest metadata, queued/starting status, missing records, exit codes and log reads without parsing arbitrary payload text. State must distinguish a missing record from an exited process. Preserve legacy job records where safely possible.
- [ ] Implement a bounded wait that does not stop the job and returns a still-running state when the wait expires. Never block the async runtime on synchronous polling/sleep.
- [ ] Bind opaque cursors to VM identity/generation, job ID, log identity and byte offset. Cap output per reply, advance only by returned bytes, preserve UTF-8 boundaries, and handle zero bytes and rotated/shrunk logs without silent loss. Reject malformed/cross-job cursors and mark explicit resets.
- [ ] Test repeated reads, multibyte boundary handling, replay/wrong-job cursors, missing/queued logs, rotation, and no cursor tail mode. Include schema checks for errors and normal states.
- [ ] Run targeted tests, fmt and clippy. Do not commit.

### Task 3: Fail-closed read-only inspection

**Files:** src/mcp.rs. Unit/protocol tests alongside implementation.

**Interfaces:** inspect_desktop(Parameters<DesktopArgs>) -> CallToolResult; same name/tool/arguments input as use_desktop_tool. Declared read_only_hint=true, open_world_hint consistent with observation behavior. Share the existing output/error/structured-content gateway handling rather than duplicating it.

- [ ] Write failing dispatch tests: explicitly read-only inner tool accepted; false/missing annotation/unknown tool refused; click/type refused without invoking the inner mutation; reconnect invalidates classification.
- [ ] Add the route and guard that validates the active guest driver's metadata before dispatch. Test cached metadata only if its generation remains valid; a retry/reconnect must not authorize using an obsolete classification.
- [ ] Refactor only enough shared code to preserve existing desktop image/structured-content/error behavior for both routes. No mutation retries and no fallback from inspect to use.
- [ ] Verify tools/list announces the new route and accurate annotations; existing route stays destructive. Include annotations in individual driver schema replies if needed for discoverability.
- [ ] Run targeted tests, fmt and clippy. Do not commit.

### Task 4: Documentation, end-to-end checks, and workflow measurements

**Files:** README.md, AGENTS.md, plugin/skills/agentpc/SKILL.md, site/mcp.html, site/index.html; src/mcp.rs instructions; scripts/smoke.py; scripts/check.sh only if new tracked test files require wiring. Add a focused Python integration script if needed.

**Interfaces:** Document the exact fields chosen in Tasks 1-2, get_job_status optional parameters, inspect_desktop safety and compact observations supported by pinned driver. Tool count becomes 22 if inspect_desktop is the only added tool; derive count from the source, don't assume.

- [ ] Replace timeout claims that merely say a command is killed with confirmed/unconfirmed termination semantics and execution-handle recovery.
- [ ] Update all tool tables, count, examples and client-facing instructions. Show backward-compatible no-wait usage and bounded-wait/cursor usage. Document UTF-8 cursor/reset and output caps accurately.
- [ ] Add protocol/VM tests for actual termination and unrelated processes on Linux/Windows, partial stdout/stderr, structured nonzero exits, wait timeout leaving job alive, sequential log deltas without duplication, and inspect refusal of mutation.
- [ ] Keep command/test output free of viewer credentials. Run fresh task-owned Ubuntu/Arch/Windows VMs sequentially and delete them on every exit path. Existing images are available; do not build new ones.
- [ ] Run scripts/check.sh and scripts/smoke.py --bin target/release/agentpc --os ubuntu,x86apps,arch,arch-x86apps,windows with a Monitor. Run lifecycle-stress.py if lifecycle/ownership implementation changed.
- [ ] Record tool calls, bytes and elapsed time for representative execution/observation workflows; distinguish unmeasured expectations from measured changes.
- [ ] Request broad correctness/spec review of all uncommitted changes. Fix confirmed important findings, rerun covering tests, then report completed/skipped/failed work accurately. Leave the branch and edits uncommitted.
