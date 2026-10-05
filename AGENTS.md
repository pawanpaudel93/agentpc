# agentpc — instructions for coding agents

This repo runs instant, resettable Windows, Ubuntu and Arch Linux ARM desktop VMs on an Apple Silicon Mac and
exposes them to you through one MCP server, `agentpc` (`agentpc mcp`). It is
preconfigured for Claude Code (`.mcp.json`), Codex (`.codex/config.toml`), Gemini CLI
(`.gemini/settings.json`), Cursor (`.cursor/mcp.json`) and VS Code (`.vscode/mcp.json`),
all launching the installed `agentpc` binary. If the `agentpc` tools are missing, the user
can run `agentpc mcp-install` (or install first: see README.md).

## Using the VMs

| Tool | Use |
| --- | --- |
| `list_vms` | VMs (owner, state, size, checkpoints, viewer and, for running x86apps VMs, `x86_tso`) and the images (with OS version) they come from. Start here. |
| `create_vm(os, version?, name?, memory_gb?, cpus?, offline?, open_in_browser?)` | New clone: ubuntu/arch ~1 s, windows ~4 s (resumed from a snapshot). Returns when ready; `name` is up to 64 letters, digits, `.` `-` `_`. Retrying with the same `name` and image in the same session returns the VM it already made (booting it if stopped). `memory_gb` 2–64, `cpus` 1–16 (default 4). `offline` cuts off the internet. `open_in_browser: true` opens the ready VM’s viewer in the Mac’s default browser (default false; not a saved VM setting). A browser-launch failure warns without failing creation. |
| `start_vm` / `stop_vm` / `reset_vm` / `delete_vm` | Lifecycle. `reset_vm` = back to a clean install. |
| `checkpoint_vm(name, label)` / `restore_vm(name, label)` / `delete_checkpoint(name, label)` | Save disk + memory before a risky step; restore in seconds; or drop one checkpoint. |
| `take_screenshot(name, save_to?)` | Hypervisor screenshot; works even while booting or hung. `save_to` also writes the PNG to a Mac path. |
| `run_command(name, command, timeout?, background?)` | Shell over SSH: PowerShell on windows, bash on ubuntu and arch. Returns a typed result (`structuredContent`, see "Running commands") plus the readable `exit code: N` / stdout / stderr text; each stream is trimmed to its first and last 10,000 characters. At a foreground run's `timeout` (default 120 s) agentpc attempts to stop it, and the reply says whether that was confirmed; `background: true` returns a job id you poll with `get_job_status`. |
| `get_job_status(name, id, tail_lines?, wait_seconds?, cursor?)` | State of a background job (queued, running, exited with its code, stopped, ended without an exit code, or missing) plus its log, as a typed result. `wait_seconds` (0 by default, at most 30) waits without ever stopping the job; `cursor` reads the log incrementally. Also takes the `job_id` of a foreground run whose outcome was uncertain. See "Watching jobs". |
| `stop_job(name, id)` | Stop a background job, or a foreground run's `job_id`, and everything it started (on Linux its systemd scope, which processes that detach stay in, when the guest has a user manager, else the processes marked with the job and their descendants; on Windows its process tree). For a job that already exited, it stops only what the job left running. |
| `upload_file` / `download_file` | Copy files or folders between the Mac and a VM. |
| `forward_port(name, guest_port, host_port?, protocol?)` | Reach a server in the VM from the Mac at `127.0.0.1:<host_port>` (a free port if omitted). TCP (default) is an SSH tunnel: reaches a server on the guest's own `127.0.0.1`. `protocol: "udp"` is a QEMU host forward: the guest server must listen on `0.0.0.0`; not on offline VMs. Lasts until the VM stops. |
| `list_forwards(name)` / `delete_forward(name, host_port, protocol?)` | List a VM's forwards (with protocol) / stop one. |
| `read_vm_log(name, which, tail_lines?)` | Tail a VM's `qemu` or `serial` log when it won't boot or the desktop is unreachable. |
| `list_desktop_tools(name, tool?)` | Desktop-control tools in that VM with their required arguments and read-only marks, or one tool's full schema (with its annotations). |
| `use_desktop_tool(name, tool, arguments?)` | Call one of those tools (click, type, launch, snapshot…). Wrong arguments or a wrong name return the argument list or close matches. Marked destructive. |
| `inspect_desktop(name, tool, arguments?)` | The read-only twin of `use_desktop_tool`, with the same arguments: it runs a tool only if that VM's driver explicitly marks it read-only, and refuses anything else before dispatching it. See "Looking without acting". |

Work in a loop: look (`take_screenshot`, or `inspect_desktop` with a snapshot tool) → act (`use_desktop_tool`) →
look again to verify. Prefer `run_command` for anything a shell can do; use `use_desktop_tool` for GUI-only work.

**Windows** (desktop tools from cua-driver): `launch_app` takes a name such as `notepad` and
returns the `pid` and `window_id`s. `get_window_state(pid, window_id)` returns numbered elements
and a `snapshot_id`; `click`/`type_text` take `snapshot_id` with `element_index`. Typing into the
focused field, scroll, drag and right-click often need `"delivery_mode": "foreground"`.

**Ubuntu** (desktop tools from cua-driver, XFCE on X11): `get_desktop_state` returns a
screenshot plus window pid/window_id values to pass to other tools. Keyboard and mouse
tools need `"delivery_mode": "foreground"`. `launch_app` takes a command name such as
`xfce4-terminal`. Google Chrome is installed: `launch_app` `google-chrome` with the URL in
`additional_arguments`, then `get_window_state`, reads a page; the `browser_*` tools drive one
(the MCP server's instructions and the plugin skill give the call sequence). Opening a `.deb`
(a double-click, or `xdg-open`) installs it with apt in a terminal window that closes on success.

**Arch** (Arch Linux ARM, a community port of Arch; same XFCE desktop and tools as Ubuntu):
the browser is Chromium, so `launch_app` `chromium` where Ubuntu uses `google-chrome`.
Install packages with `sudo pacman -Syu --noconfirm <pkg>` (Arch doesn't support partial
upgrades, and an image's package lists age); the first `-Syu` may upgrade the whole system, so
give `run_command` a longer `timeout` or `background: true`, and after a kernel upgrade reboot
with `start_vm` before loading new modules. `/tmp` is a tmpfs, cleared at every boot.

`list_desktop_tools` shows each tool's required arguments, and a call with wrong arguments returns
the tool's argument list (a wrong tool name returns close matches). If `get_window_state` comes
back "degraded" with no elements, act by pixels (`x`/`y` from that call's screenshot). The driver
is pinned per image; don't update it inside a VM. If a reply starts with a reconnect note (the VM
or its driver restarted), earlier snapshot ids and browser sessions are gone: take a new
snapshot and run `browser_prepare` again.

### Running commands

`run_command` replies with `structuredContent` (declared as the tool's `outputSchema`,
`schema_version` 1) and one text block. The text is what scripts have always read: `exit code: N`
then `--- stdout ---` / `--- stderr ---` sections, or `started in the background (id N, ...)`. It is
not a JSON copy, so a client that joins text blocks gets exactly that. Read the typed fields, not
the text, and trust `isError`:

- `state`: `exited` (it ran to its end; `exit_code` is its code), `timed_out`, `not_started` (it never
  ran), `connection_lost` (SSH failed while it may have been running) or `started` (a background job).
- `stdout`, `stderr` (the trimmed text), `stdout_bytes`, `stderr_bytes`, `stdout_truncated`,
  `stderr_truncated`, `output_truncated` (whether any part was left out; write big output to a file
  and `download_file` it).
- `timed_out`, `termination_confirmed`, `effect_unknown`, `recovery_action`, `error`.
- `execution_id` (issued to each run; null only when the call failed before one was, such as an unknown or stopped VM), `job_id` (set for a background job, and for a foreground
  run whose fate is uncertain: it is the handle for `get_job_status` and `stop_job`) and `log_path`.

`isError` is true for every nonzero exit, every timeout, and every run that did not start or lost its
connection. It is false only for exit code 0 and for a background start. A timeout is an error even when
the command's own exit code is known to be 0.

Foreground exit codes are what SSH sees, the low 8 bits (on Windows `exit -1` and `exit 511` read 255).
Background jobs keep the code their wrapper recorded.

**Timeouts.** At `timeout`, agentpc tries to stop the command and what it started, and keeps the
partial stdout and stderr:

- `termination_confirmed: true` means agentpc verified that none of the processes it identified is
  left. On Linux those are the command's transient systemd user scope when the guest's user manager is
  available, the processes marked with its execution id and their descendants (all that is checked when
  there is no user manager, where a process that clears its environment and detaches can be missed), and
  its verified root. On Windows it is the verified process tree, best effort: a process whose launcher had
  already exited can be missed. It is not a sandbox guarantee. System services a command starts
  (`systemctl`), other systemd units, and Windows services, scheduled tasks and WMI-created processes are
  not covered.
- `termination_confirmed: false` or `effect_unknown: true` (which includes `connection_lost` with
  `effect_unknown: true`) mean it may still be running. `job_id` is set: call `get_job_status` and, if
  needed, `stop_job` with it. A `connection_lost` with `effect_unknown: false` is different: the VM
  recorded the command's exit (`exit_code`), so only its output may be incomplete, and `job_id` is null.
  A guest process is only signalled after its pid and start time are proven to be the command's, never
  by a bare pid.
- The cleanup is bounded. The reply usually follows the timeout within a few seconds, and in the worst
  case about 50 seconds later.
- A command whose exit code the VM recorded, but whose run had not ended by the timeout (a child it left
  may have held its output open, or it ended right at the deadline), comes back with that exit code and
  `timed_out: true`; agentpc's cleanup then verified that none of the processes it identified remains
  (`termination_confirmed: true`). Start servers with `background: true`.
- `recovery_action` says what to do next: `get_job_status` or `stop_job` (with `job_id`), `start_vm`,
  `inspect_vm` (look at the VM before running it again), or `retry`, only when the command provably
  never ran. agentpc never retries a command itself. Don't retry one whose `effect_unknown` is true
  until you have checked what it did.

A foreground command on Linux is limited to 92 KiB (94,208 bytes of UTF-8) of command text: it travels
base64-encoded in one SSH command line and once more in an environment string, each capped by the kernel
at 128 KiB. agentpc checks the size before sending anything: a bigger command comes back `not_started`
with no `execution_id` and no `recovery_action` (the same command would fail again). For a bigger script,
`upload_file` it and run the file. Background jobs are not held to this number.

### Watching jobs

`get_job_status(name, id, tail_lines?, wait_seconds?, cursor?)` takes a background job's id, or the
`job_id` of a foreground run. Its `structuredContent` (`outputSchema`, `schema_version` 1) has `state`
(`queued`, `running`, `exited`, `stopped`, `ended`, `not_started`, `missing`, `unknown`), `exit_code`,
`kind` (`background` or `foreground`), `stop_reason`, `log_tail` or `output`, `output_bytes`,
`output_truncated`, `invalid_utf8`, `has_more`, `next_cursor`, `cursor_reset`, `cursor_reset_reason`,
`wait_timed_out`, `waited_ms`, `note`, `error` and `error_code`. The text keeps the `STATE: ...` line
and `--- log tail ---`.

- With neither `wait_seconds` nor `cursor` it behaves as it always did: it answers at once with the last
  `tail_lines` (default 50) lines, at most 16 KiB of them, and now also a `next_cursor` at the end of
  what that call read.
- `wait_seconds` (0 to 30, default 0; more is an `invalid_wait_seconds` error) waits for the job to
  leave `running`, or, with a `cursor`, for new output or a reset. If neither happens it returns the
  current state with `wait_timed_out: true`. A wait only waits: it never stops the job. A slow VM can
  make the reply run about 2 seconds past the wait; if the check at the deadline does not answer, the
  last good state comes back with a note saying so. To wait longer, call again with the last
  `next_cursor`.
- `cursor` is a `next_cursor` from an earlier reply about this VM and job, or `"start"`. It is opaque:
  don't build or edit one. A reply holds at most 16 KiB of log and its `next_cursor` advances by exactly
  the bytes it returned, so repeated reads neither repeat nor skip output; `has_more: true` means call
  again. A UTF-8 character cut by the end of a read is held for the next one, invalid bytes become
  U+FFFD with `invalid_utf8: true`, and a UTF-8 byte-order mark is consumed without being shown.
- When a cursor no longer matches (the VM was reset or restored, the guest rebooted, or the log was
  replaced, shortened, rewritten or removed), the reply has `cursor_reset: true` and a
  `cursor_reset_reason` (`vm_changed`, `reboot`, `log_replaced`, `log_shrunk`, `log_rewritten`,
  `log_missing`), and restarts at the beginning of the current log: discard what you assumed.
  Detection is best effort. It can miss a guest clock step, a Windows log recreated within the file
  system's tunneling window, and a log truncated and rewritten with the same first 64 bytes that is at
  least as long as the cursor's offset.
- `missing` means nothing in the VM has recorded itself under that id (yet). That does not prove the
  command never started, so don't blindly retry it: a slow launch may still arrive. A VM that can't be
  reached is an error with `state: unknown` and an `error_code` (`guest_unreachable`, `bad_guest_reply`,
  `no_such_vm`, `vm_not_running`, `invalid_cursor`), never `ended`.
- An `exited` job reports its own exit code even if it left a child running (`stop_job` stops that child).
- A foreground handle has status only (`kind: foreground`: exited with its code, stopped at its timeout,
  or not started). Its output went to the `run_command` reply, so there is no log to replay.

### Looking without acting

`inspect_desktop(name, tool, arguments?)` takes the same arguments as `use_desktop_tool` but is annotated
read-only (not destructive, open-world: what it returns can include web page text). Before each
dispatch it asks the VM's driver for the tool's annotations and runs the tool only if the driver
explicitly says `readOnlyHint: true` (`list_desktop_tools` shows `[read-only]`). A tool the driver marks
otherwise, leaves unmarked, or doesn't know is refused before it is dispatched, with nothing run (looking up the annotations does ask the VM's
driver). After a
dropped call the driver is asked again on the new connection, so an old classification is never reused,
and the call is re-sent only if the connection it was sent on had listed the tool read-only and the new
one still does. The driver's marking is its own claim: a guard against mistakes, not against a driver or
guest that mislabels a tool.

Observation can be compact. `get_window_state` with `include_screenshot: false` returns the element tree
without a screenshot, and its `query` argument narrows what comes back (`list_desktop_tools` with `tool`
shows the pinned driver's full schema). `use_desktop_tool` is unchanged: it is still the route for tools
that act, it is marked destructive, and driver updates inside a VM stay blocked.

### Guest tips

- **Networking.** VMs are isolated behind user-mode NAT. From a guest, `10.0.2.2` is the Mac
  host (a Mac server on `127.0.0.1`/`0.0.0.0` is reachable at `10.0.2.2:<port>`). For VM-to-VM,
  `forward_port(B, guest_port, host_port)` then connect from A to `10.0.2.2:<host_port>`; for
  UDP pass `protocol: "udp"` and have B's server listen on `0.0.0.0` (a Windows guest also needs
  a firewall rule for it). An offline VM can't reach `10.0.2.2` but forwarded TCP ports still
  work (UDP forwards don't: its network drops the guest's UDP). Every VM's own address is
  `10.0.2.15`, so a service that advertises its address advertises the same one from each VM;
  peers elsewhere reach it only through a forward. A guest inherits no Mac
  proxy; set `HTTP(S)_PROXY` and import a corporate CA in the guest itself.
- **Reboots** (Windows Update, some installers) drop SSH: call `start_vm` (it waits until the
  VM is ready again), then rerun the command. If its reply was `connection_lost` with
  `effect_unknown: true`, check it first with `get_job_status` and its `job_id`.
- **Windows:** GUI installers return at once — `Start-Process x.exe -ArgumentList '/S' -Wait
  -PassThru` and check `ExitCode`. Windows Update is disabled (`wuauserv` off + `NoAutoUpdate`),
  which blocks DISM/`Add-WindowsCapability`; re-enable with `Set-Service wuauserv -StartupType
  Manual; Start-Service wuauserv`, then set it back to `Disabled`. Defender real-time protection
  is on (only SmartScreen is off) and may quarantine test binaries — `Add-MpPreference
  -ExclusionPath C:\work`. The guest is ARM64; x64 and x86 programs run through Windows' Prism
  emulation (roughly 2–4× slower), but x64 drivers and kernel-mode software don't. Prefer an
  ARM64 build when one exists.
- **x86 Linux programs** (x86_64 and i386) need an x86apps VM: `create_vm(os: "ubuntu" or
  "arch", version: "x86apps")`. FEX translates them to arm64, about 2× slower (JIT runtimes like
  Node 6–7×); run them directly (`./tool`). Go programs work. x86 containers work with `docker
  run --platform linux/amd64` (install Docker first: `sudo apt install docker.io` on Ubuntu,
  `sudo pacman -Syu --noconfirm docker && sudo systemctl start docker` on Arch). x86
  Electron/Chromium apps need `--no-sandbox`. A missing x86 library on Ubuntu: `sudo apt install
  libfoo:amd64`; an x86 app's `.deb`: `sudo apt install ./app_amd64.deb` (its install scripts see
  an x86_64 machine, so ones that check `uname -m` pass). Arch has no multiarch: x86 programs there use FEX's x86 Arch Linux tree, and
  `sudo fex-pacman -Sy --noconfirm --needed <pkg>` installs more x86 packages into it.
  x86 systemd services run too, hardened ones included: for a unit whose `ExecStart` is an
  x86 program, a generator relaxes `MemoryDenyWriteExecute=` and `LockPersonality=` (they stop
  FEX, as they stop any JIT). If a unit runs its x86 program another way (a script) and dies
  at start with a SIGSEGV inside FEX, `sudo fex-unit <unit>` does the same. A big x86 program
  takes longer to get going on its first start (FEX translates it; later starts reuse the
  cache), so a health check right after `systemctl start` may need a retry. An installer that
  refuses non-x86_64 (`uname -m`) runs
  unmodified under the x86 bash: `sudo FEXBash ./install.sh` (its `uname` and tools then run as
  x86 programs). For a paste block (`curl ... | sudo bash -s`), start `FEXBash` and paste it
  there: FEXBash's `sudo` keeps the command x86.
  `list_vms` shows `x86_tso`: `hardware` (fast; needs macOS 15+) or `emulated`.
- Guests are 1280x800 with a 2D-only GPU (no acceleration) and no audio device.

### Rules

- Instances are resettable; create your own uniquely named VM per task and `reset_vm` it
  instead of repairing a broken one. Don't reset, stop, restore or delete a VM you didn't
  create unless the user asks.
- Don't create instances you won't use, and `delete_vm` your instances when done.
  Each running VM takes 4 GB (ubuntu, arch) or 8 GB (windows) of RAM.
- `create_vm ubuntu` downloads the Ubuntu image on first use (~1.2 GB); pass `version`
  (e.g. "22.04") for another release, or `version: "x86apps"` (`ubuntu-24.04-x86apps`; also
  "22.04-x86apps": x86apps is for 22.04 and 24.04 only) for Ubuntu that also runs x86 Linux programs (downloaded on first use, or built locally, ~8 min,
  if the download fails). `create_vm arch` (`arch-rolling`; `version` may be `x86apps`, or pin
  a published build, `rolling-YYYYMMDD` / `rolling-x86apps-YYYYMMDD`) downloads the Arch image
  on first use, or builds it locally (~6 min; ~10 min for x86apps) if the download fails.
  A dated version (`24.04-YYYYMMDD`, `rolling-YYYYMMDD`, …) is download-only, never built.
  A Windows image must be built by the user once:
  `agentpc image build windows` (downloads the ISO; ~12 min), or another version
  (`windows-11-24h2`, `windows-11-23h2`; `agentpc image build --help` lists them).
  If `list_vms` shows no windows image, ask the user to run that. Don't start a build
  yourself unless asked.
- Open the host browser only when the user asks to watch: `create_vm` with
  `open_in_browser: true`. Omit it otherwise; the viewer is still available. Retrying with
  it set can open another tab; it is an action, not part of the VM’s saved configuration.
- The login for every guest is `agent` / `agent` (desktop and sudo; SSH takes only agentpc's
  key). Everything binds to 127.0.0.1.
- VMs you create, start, reset or restore over MCP are stopped (never deleted) when the
  session ends, unless `AGENTPC_KEEP_RUNNING=1`. If a session's server is killed instead, the
  next agentpc MCP server to start stops them. `list_vms` shows `owner_running: false` for a VM
  whose session is gone; if you created it (a restarted session), `create_vm` with its name and
  image takes it back.
- If a tool's options look older than what agentpc does (an OS or image `list_vms` shows that
  `create_vm` doesn't list), your tool definitions predate an agentpc update: reconnect the
  agentpc MCP server or start a new session. `list_vms` says when the binary was replaced.
- An image in `list_vms` with `outdated` set was captured by an older agentpc, so it lacks newer
  guest fixes; tell the user the command it gives (don't run it yourself; it needs that image's
  VMs deleted).

## Working on this repo

- Commits follow Conventional Commits: `feat:`, `fix:`, `docs:`, `refactor:`, `test:`,
  `ci:` or `chore:`, then a short imperative subject; add a brief bullet body only if needed.
- Rust, single binary `agentpc` (CLI + MCP server). `src/main.rs` is the CLI; modules:
  `instance` (VMs, images, on-disk layout), `qemu`, `ops` (lifecycle),
  `viewer` (browser viewer), `image` (build/snapshot), `registry` (pull/push), `setup` (`doctor`, `mcp-install`, `clean`, `uninstall`), `update` (self-update), `mcp` (the server).
- Ports derive from the instance slot n: SSH 47000+n, noVNC websocket 47200+n,
  VNC 47300+n; the shared browser viewer is on 8100.
- Guest assets in `guests/` are embedded in the binary. A Windows build writes them to a
  FAT `setup.img`: `Autounattend.xml` drives Setup, `SetupComplete.cmd` runs after it, and
  `oem/setup.ps1` runs at first logon; `guests/ubuntu/user-data` is the Ubuntu cloud-init.
  An Arch build runs `guests/arch/build.sh` in a clone of the Ubuntu image, installing Arch
  Linux ARM onto a blank second disk (`/dev/vdb`) that becomes the image.
  `guests/<os>/prepare.*` runs in the guest every time a snapshot is captured (agent defaults:
  no pop-ups or updates, Chrome on Ubuntu, Chromium flags on Arch), so it also upgrades
  existing and pulled images.
  On `ubuntu-<release>-x86apps` images (22.04 and 24.04, which have a pinned x86 root filesystem), `guests/ubuntu/x86apps.sh` runs first: FEX (tag and
  commit pinned there) built static-pie from source with `guests/ubuntu/fex.patch` (so x86
  containers work; bumping the pin may need the patch rebased), its x86 root filesystem,
  amd64 apt sources, and `agentpc-fex-tso`. On `arch-rolling-x86apps`, `guests/arch/x86apps.sh`
  does the same with the same patch and an unpacked x86 Arch Linux root filesystem (no multiarch;
  `fex-pacman` installs x86 packages into it in a chroot). QEMU for those VMs loads `src/hvf_tso.c` (built by
  `build.rs`) to turn on the CPU's TSO mode; after each boot `ops` tells FEX which mode it got.
  Shell helpers the Linux guest scripts install (the FEX systemd generator, FEXBash's `sudo`,
  `fex-unit`, the dpkg maintainer-script wrappers, the `.deb` opener) are files in
  `guests/helpers/`, uploaded to `/tmp/agentpc-helpers` before the scripts run and tested
  without a VM by `scripts/test-guest-helpers.sh` (CI runs it with dash and bash).
  Changes take effect on the next `agentpc image build`, which refuses while VMs of that image exist.
  What guests download is pinned: FEX by tag and commit, the x86 root filesystems and
  cua-driver's installers and binaries by sha256. Bumping cua-driver means `CUA_DRIVER_VERSION`
  in `image.rs` plus the version and sha256s in each guest's setup (a unit test checks the
  versions); builds and snapshots refuse a guest running another version.
- Images are `<os>-<version>` (`Image` in `instance.rs`); a bare OS means its default version.
- State (images, keys, instances) lives in `~/.agentpc` (`AGENTPC_HOME` overrides).
- `plugin/` is the Claude plugin (MCP server + `skills/agentpc/SKILL.md`), listed by
  `.claude-plugin/marketplace.json`. Keep the skill's tool guidance in sync with the
  "Using the VMs" section above; check with `claude plugin validate plugin --strict`.
- Verify with `scripts/check.sh` (every CI check: fmt, clippy, tests, shellcheck, the guest
  helper tests, the Arch power-key test, static-site SEO/link checks, VM test script syntax and
  plugin validation;
  `release.sh` runs it too) plus a real instance (`agentpc create
  ubuntu`, then the MCP tools). Unit tests can't cover the VM paths. After changing `guests/`, the desktop
  driver or the MCP gateway, rebuild the affected images and run `scripts/smoke.py --bin
  target/release/agentpc`: it drives fresh VMs through the MCP server like an agent does.
- For lifecycle or ownership changes, also run `python3 scripts/lifecycle-stress.py --bin
  target/release/agentpc --rounds 3`. It uses fresh task-owned Ubuntu VMs from an existing,
  current snapshot, never downloads/builds images, and deletes its VMs afterward. It refuses
  to start when unrelated running orphan VMs could be reaped; do not bypass that guard.
- Never commit `target/`.
