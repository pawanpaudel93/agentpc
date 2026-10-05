# Execution and observation design

Approved in conversation on 2026-10-04. Scope: four improvements only.

## Requirements

1. Preserve foreground command behavior, including separate stdout/stderr, desktop-session environment, bounded head/tail output, PowerShell/native exit semantics, and synchronous completion. Background commands remain detached.
2. Track foreground execution with a server-issued ID and verified guest-process identity. A timeout attempts to terminate that execution and its children, never an unrelated PID/session. Return partial output, the execution ID, and whether termination was confirmed. Distinguish timeout, connection failure, nonzero exit, and unconfirmed effects. Never automatically retry a command.
3. Add stable structured output to run_command and get_job_status, with readable text retained. Nonzero command exits and failed execution remain tool errors. Both success and failure have a documented schema. Avoid parsing human-format errors into structured outcomes.
4. Extend get_job_status with optional wait_seconds (0 by default, max 30) and cursor. Omitted cursor keeps current tail_lines behavior. A wait only waits, never cancels. An expired wait returns current state. Cursor reads return bounded new output, a next cursor, and a reset indicator if underlying log identity/generation changes. Handle invalid cursors, log shrink/rotation, Unicode, missing files/queued jobs, and VM reboot/reset safely. Preserve exited-code semantics when an exited job left a child running.
5. Add inspect_desktop with the same input shape as use_desktop_tool, marked read-only. Only guest tools explicitly marked read-only may be dispatched. Unknown/missing classification fails closed. Reconnection must invalidate classifications. Existing use_desktop_tool remains available and desktop image/structured-content/error behavior must remain compatible.
6. Document compact tree-only/query observations already supported by the pinned driver; do not invent new driver options.
7. Update README, AGENTS, plugin skill, site MCP docs/tool count, server instructions, and tests consistently.

## Non-goals

No GUI, service-readiness feature, prepared-environment forks, file sync, action batches, MCP task protocol dependency, automatic mutation retries, new image builds, commits, pushes, or releases. Existing uncommitted research notes must stay intact.

## Implementation boundaries

Keep the low-level exec used for infrastructure probes separate from tracked user-command execution. Reuse existing job containment where safe, but do not silently turn every short foreground command into an expensive scheduled background task. Guest metadata/IDs must never rely on a bare reused PID; a recorded start time/boot generation or equivalent proof is required before signalling.

Metadata must be recorded before running payload code. A termination confirmation requires verifying no identified execution processes remain, not merely observing that the local SSH process exited. Cleanup uses a bounded deadline. If the SSH connection or guest state prevents proof, report termination unconfirmed plus an execution handle rather than claiming stopped.

Structured data must describe output truncation accurately. Preserve the existing human-readable text fallback useful to scripts (including exit-code and STATE labels) alongside structuredContent. Do not append a separate serialized-JSON text block: existing text-only consumers concatenate blocks and would receive polluted stdout. MCP recommends the JSON mirror with SHOULD, not MUST; this compatibility choice was clarified during Task 1. Tests validate both surfaces and error hints.

## Verification

Baseline scripts/check.sh passes: 69 tests, 2 ignored. Branch feat/execution-observation was created because the native worktree hook returned no path.

Unit tests cover schemas, serialization/error results, output bounds, cursor binding/reset, Unicode handling, wait bounds, and fail-closed inspection. Protocol tests cover declared tool schemas and result content. Fresh task-owned Ubuntu, Arch and Windows VMs verify actual foreground timeout termination, child/root/detached cases, partial output, unrelated-process survival, background waits/cursor continuity and zero-wait compatibility, and read-only route refusal of mutations. Use monitors for builds/long runs. Do not print viewer URLs or credentials. Delete task-owned VMs in finally cleanup.

Measure selected workflows' calls, result bytes and elapsed time without claiming improvements before comparison. Full check.sh and smoke.py must pass on the final tree. Test lifecycle stress if lifecycle/ownership paths change. No image rebuild is authorized or needed unless guest assets change; avoid guest-asset changes for this feature.
