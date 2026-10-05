# Execution recovery and readiness design

Approved in conversation on 2026-10-05. Baseline `8d3c660`, same branch `feat/execution-observation`.

## Scope

Three additions: foreground client-request cancellation cleanup; metadata-only job discovery; bounded guest-loopback TCP/HTTP readiness. No stronger Windows containment, prepared-environment forks, sync, GUI, commits, merge, push, release, image builds, downloads or guest-asset changes. Existing research notes remain untouched.

## Cancellation contract

Foreground execution and verified cleanup belong to an owned supervisor, not only the request future. `run_command` observes the exact rmcp 3.4.1 RequestContext cancellation token. A cancelled request or dropped handler signals bounded cleanup even when the handler cannot return a result. Normal completion disarms cleanup. Explicit background start requests retain their existing detached semantics; cancellation after start must not kill intentionally background work. Cancel before payload dispatch may prevent a start, but never replay or report prevention without proof.

Reuse process/cgroup identity and tombstone protections; no bare PID signalling, no automatic payload retries. Record request-cancelled termination distinctly from deadline timeout so job discovery/status can explain it. Preserve current readable text, output bounds and existing success/error outcomes. Any new cancelled outcome must have a documented schema and tool-error semantics, without falsely claiming confirmation when transport/cleanup fails.

Keep control state thread-safe, do not hold locks across awaits, and bound active supervision/cleanup retention. Integrate normal server shutdown so cleanup is signalled/drained within the existing shutdown budget; do not add sequential 45-second waits that double it. Existing VM ownership and `AGENTPC_KEEP_RUNNING` policy remain intact. Hard SIGKILL/runtime loss cannot guarantee immediate cleanup; persisted records permit later recovery.

## Job discovery contract

Add `list_jobs(name, limit?, cursor?)`. Read-only metadata, newest ID first, default20/max100, bounded/paginated results. Include execution/job ID, foreground/background/unknown kind, state, known exit code, log availability and safe optional timestamp metadata only when actually known. No payload scripts, command text, environment values, stdout/stderr, full logs, or secret-derived contents.

Reuse existing job-state/identity rules. Missing/unrecorded IDs and transitional `.fg`/PID/proof records do not prove never-started; never suggest blind retry. Page cursors bind VM generation and deterministic enumeration position; concurrent new jobs must not duplicate or skip the original traversal. Invalid/foreign/stale cursors have explicit structured handling. Bound enumeration and reply size; avoid one expensive SSH call per item. Legacy records remain discoverable where safe; aged/deleted records can disappear and must not be presented as a complete immutable history.

## Service readiness contract

Add `wait_for_service(name, guest_port, protocol?, timeout_seconds?, path?, expected_status?, job_id?)`, default TCP, timeout30/max120 seconds. Targets only that VM's `127.0.0.1`, not arbitrary hostnames/URLs. Port must be1..65535. HTTP uses an explicit relative path (default `/`), GET, expected status200 by default, no redirects, no response body in the result, no arbitrary headers/auth credentials. A TCP connection proves only listener acceptance; an HTTP match proves only the configured status criterion.

Use existing SSH/network machinery and standard guest capabilities without installing dependencies or creating lasting forwards. Validate/escape arguments, cap each connection/HTTP attempt and total wait, use a monotonic async deadline and cancellable waits. Return typed observed criterion/status, attempt count, elapsed time and a clear timeout/error. Waiting never kills or restarts the service/job. Optional job_id permits early failure on a known nonzero exit, stopped/cancelled or proven never-started job; a successful wrapper exit with a surviving server is not failure, and a missing record is not proof of failure. Clearly distinguish endpoint readiness from association with a particular process.

## Verification

Tests first: cancelled-token and handler-drop paths, cancellation before start and completion races, no replay/unrelated-process termination, intentionally background work surviving start-RPC cancellation, bounded shutdown/cleanup; list pagination and concurrent creation/deletion, missing/legacy/transitional records and no content leakage; TCP refusal/listener readiness, HTTP mismatch/match/redirect behavior, invalid paths/ports/URLs, bounded/cancelled waits and optional job failure semantics.

Exercise actual MCP cancellation notifications and disconnects, not only local future mocks, using fresh task-owned Ubuntu/Arch/Windows VMs from existing images. Check guest-side process termination and unrelated process survival. Refuse unrelated running orphan VMs before MCP launches; never bypass. Delete exact task-owned VMs in finally cleanup. Run check.sh, extended smoke on existing five variants, and lifecycle-stress if shutdown/ownership paths changed (shutdown integration likely qualifies). Keep monitors armed on long jobs; sanitize all failure/log output before persistence. Never print viewer URLs or credential values.

Record calls/result bytes/elapsed times without claiming unmeasured cost savings. Per-task/scoped reviews and one final branch review. Leave all new changes uncommitted pending user request.
