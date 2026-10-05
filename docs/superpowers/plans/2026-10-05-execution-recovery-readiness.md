# Execution Recovery and Readiness Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development or superpowers:executing-plans task-by-task. No commits authorized for this round.

**Goal:** Recover and cancel foreground work reliably, discover its records, and wait for guest services using explicit readiness criteria.

**Architecture:** Owned execution supervision independent of RPC handler lifetime, existing identity-safe guest cancellation, bounded batch job metadata queries, and guest-loopback probes with async deadlines.

**Tech Stack:** Rust, rmcp3.4.1, Tokio1.53.1, serde/schemars, standard Linux and Windows PowerShell5.1 guest facilities. Existing reqwest0.13 only if appropriate; avoid unnecessary dependencies.

## Global Constraints

- Approved design: docs/superpowers/specs/2026-10-05-execution-recovery-readiness-design.md.
- Stay on feat/execution-observation, baseline8d3c660. No new worktree/branch, commits, merge/push/release, guest assets or new images/downloads.
- Preserve background detachment, VM ownership and existing session shutdown/AGENTPC_KEEP_RUNNING policy; do not double the existing shutdown budget.
- No bare PID/session signalling, automatic payload replay, or claimed termination without verification.
- Readable single text fallback and typed success/error schemas remain compatible. New states/reasons documented.
- list_jobs default20/max100 metadata-only; no command text/env/stdout/logs/payload contents.
- wait_for_service targets only guest127.0.0.1; defaultTCP, timeout30/max120; HTTP GET relativepath/default `/`, expected200, redirects off, response bodies/credentials/headers excluded.
- Waiting never kills service/job. Missing/transitional job state is not proven failure; successful wrapper exit may leave a server alive.
- Async, monotonic, bounded, cancellable I/O. No blocking waits or locks across await.
- Tests/logging value-blind/sanitized; never read/print/store credentials or viewer URLs. Use uniquely named task-owned VMs, refuse unrelated running orphan VMs, delete exact owned names in finally.
- Long checks use Monitor. Do not weaken existing tests or change prior research notes.

### Task 1: Foreground supervision and cancellation ownership

**Files:** src/execution.rs, src/mcp.rs, tests/execution.rs; optional narrow supervision module/main declaration if needed.

**Interfaces:** run_command accepts RequestContext<RoleServer> and observes ctx.ct. Preserve run_tracked/low-level infrastructure exec separation. Supervision owns process/stream capture and verified cleanup until terminal state; handler drop only requests cancellation, never abandons ownership. Gateway shutdown signals/drains outstanding workers within existing shutdown deadline, respecting VM policy.

- [ ] Write tests proving a started foreground execution receives cleanup on token cancellation and dropped handler, and normal completion disarms it. Use independent fake process fixtures like current tests/execution.rs, not spawning inside flock-sensitive main tests.
- [ ] Observe rmcp3.4.1 source behavior rather than assuming notification immediately aborts a handler. Confirm token/cancellation timing with an in-memory actual rmcp server test.
- [ ] Implement owned supervision and a drop-safe cancellation signal. Reuse bounded verified guest cancellation and preserve partial streams/known exits. Introduce a clear request_cancelled stop reason distinct from timeout/stop_job, extend output schemas conservatively.
- [ ] Ensure no background cancellation after intended detachment, and no foreground dispatch when cancellation is already known and prevention can be proved. Do not replay a payload on reconnect.
- [ ] Integrate cleanup retention/shutdown budget. Test cancelled-at-completion, never-started race, unconfirmed guest cleanup, no double cancellation, background start RPC cancellation, and server shutdown/keep-running cases.
- [ ] Run fmt, targeted tests, clippy. Report exact interfaces and remaining limitations. Do not commit.

### Task 2: Metadata-only paginated list_jobs

**Files:** src/jobs.rs, src/mcp.rs, tests/jobs.rs; narrow helper module only if truly needed.

**Interfaces:** list_jobs(Name, limit default20 maximum100, opaquecursor optional) -> typed JobList result and readable fallback, read-only annotations. Reuse JobState and process proof without mutating tombstones. IDs, kind, state, exit_code, log_available and accurate optional timestamps only.

- [ ] Add failing tests for defaults/bounds, empty directory, legacy/transitional/cancelled records, unknown/malformed records, stable descending enumeration and cursor bindings.
- [ ] Implement one bounded guest batch metadata protocol per page rather than one SSH query per job. Return only allowed fields, never read payload/log/env contents. Bound enumeration work and reply bytes.
- [ ] Bind page cursor to VM generation and initial high-water/position; new jobs after traversal start do not shift existing pages. Handle deletions safely and generation changes explicitly. Malformed/foreign cursor rejected before guest effects.
- [ ] Test multi-page traversal under additions/deletions, duplicate suffix files grouped once, requester cancellation, permissions errors and no content leakage using synthetic credential placeholders in unrequested files.
- [ ] Verify tools/list schema/annotations and structured errors, fmt, tests/clippy. Do not commit.

### Task 3: Explicit TCP/HTTP readiness

**Files:** src/mcp.rs, optional src/readiness.rs +main declaration, tests/readiness.rs if process/network fixtures required.

**Interfaces:** wait_for_service per approved design. Result contains criterion/state, observed TCP/HTTP status, attempts and waited_ms, timeout/error/cancel flag, optional relevant job state. One output schema for all results; no HTTP bodies or credential-bearing diagnostic text.

- [ ] Write failing tests for port/path/protocol/status/timeout validation, bounded polling, cancellation, TCP acceptance/refusal, HTTP expected/mismatched/redirect status and failure classification.
- [ ] Choose guest-loopback probing that uses existing standard facilities without installing packages or leaving host forwards. No arbitrary URL/hostname/headers/auth inputs; validate relativepath and prevent shell/PowerShell injection. Do not follow redirects.
- [ ] Implement monotonic async wait with capped per-attempt/overall budgets and context cancellation. Preserve a meaningful last observation on expiry; cancellation only stops probing.
- [ ] Optional job_id aborts only on known failure, cancellation/stopped or proven prevention; successful wrapper exit is compatible with readiness, missing is not failure proof. Document that endpoint matching does not authenticate its owner.
- [ ] Test real local server fixtures and generated guest scripts with failures/Unicode/escaping/slow replies; isolate spawns in a separate test binary. Verify schemas, fmt/tests/clippy. Do not commit.

### Task 4: Docs, actual protocol/VM coverage and final gate

**Files:** README.md AGENTS.md plugin skill site/mcp.html site/index.html site/llms.txt src/mcp.rs instructions/descriptions scripts/smoke.py scripts/check.sh and test-site/logging checks as needed. Keep versions/manifests unchanged.

- [ ] Update tool tables/count derived from actual registered tools (expected24 if only two tools added), cancellation outcomes/reasons and background/session semantics, list pagination/metadata retention, readiness criteria/timeouts/limits and no-mutation promises.
- [ ] Add permanent protocol tests for notifications/cancelled, dropped/disconnected clients and listing recovery. Reuse sanitized smoke client; never include raw lifecycle replies in logs.
- [ ] On fresh task-owned Ubuntu/Arch/Windows VMs, verify actual foreground/unrelated/background behavior, listing recovery/pagination, delayed TCP and HTTP readiness, explicit redirects/mismatches, timeout and cancelled probe not stopping server, and job failure shortcuts. Existing images only, clean up exact owned names.
- [ ] Run fresh check.sh, release build, all-five expanded smoke and lifecycle-stress rounds3 after shutdown integration, each with Monitor. Preserve pre-existing VMs.
- [ ] Record measured calls/bytes/elapsed without invented improvements. Per-task spec+quality and final whole-branch review; one focused final fix wave if needed, rerun covering checks.
- [ ] Leave new work uncommitted on same branch, ledger retained, and report completed/failed/skipped validation accurately.
