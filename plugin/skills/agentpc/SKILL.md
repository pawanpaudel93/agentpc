---
name: agentpc
description: Use agentpc to get an instant, resettable Windows, Ubuntu or Arch Linux ARM desktop VM on the user's Mac. Use it when a task needs a real, clean Windows or Linux machine, such as testing an installer, script or app on a fresh OS, reproducing a platform-specific bug, operating a GUI application, running x86_64 Linux programs or amd64 containers on the Mac, or taking screenshots of a desktop, and when the user mentions agentpc or asks for a Windows, Ubuntu or Arch Linux (ARM) VM.
---

# agentpc: instant, resettable desktops

agentpc runs Windows 11, Ubuntu (24.04 by default, or another release) and Arch Linux ARM
VMs locally and exposes them through the `agentpc` MCP server. A new VM is ready in about 1 s
(Ubuntu, Arch) or 4 s (Windows) and can be reset to a clean install just as fast, so treat VMs as
throwaway sandboxes.

## If the tools are missing

If the agentpc tools (`list_vms`, `create_vm`, ...) aren't available, the `agentpc` binary isn't installed or isn't on `PATH`.
Tell the user to install it (Apple Silicon Mac only), then restart the session:

```sh
curl -fsSL https://agentpc.pawanpaudel.com.np/install.sh | sh
```

## Tools

| Tool | Use |
| --- | --- |
| `list_vms` | VMs (owner, state, size, checkpoints, viewer and, for running x86apps VMs, `x86_tso`) and the available images with OS versions. Start here. |
| `create_vm(os, version?, name?, memory_gb?, cpus?, offline?, open_in_browser?)` | New VM (`ubuntu`, `windows` or `arch`, optionally a version such as `22.04`, `x86apps` or `22.04-x86apps`; x86apps is for 22.04 and 24.04 only); returns when the desktop is ready. `name`: up to 64 letters, digits, `.` `-` `_`; retrying with the same `name` and image in the same session returns the VM already made (booting it if stopped). `offline: true` cuts it off from the internet and this Mac. `open_in_browser: true` opens the ready VM’s viewer in the Mac’s default browser (default false; not a saved VM setting). A browser-launch failure warns without failing creation. |
| `start_vm` / `stop_vm` | Boot a stopped VM / shut one down |
| `reset_vm(name)` | Discard all changes: back to a clean install |
| `checkpoint_vm(name, label)` / `restore_vm(name, label)` / `delete_checkpoint(name, label)` | Save the VM's disk and memory; go back to that state in seconds; or drop one checkpoint |
| `delete_vm(name)` | Delete a VM with its disk and checkpoints |
| `take_screenshot(name, save_to?)` | PNG screenshot; works even while booting or hung. `save_to` also writes it to a Mac path |
| `run_command(name, command, timeout?, background?)` | Shell command: PowerShell on Windows, bash on Ubuntu and Arch. Returns typed `structuredContent` (`state`, `exit_code`, `stdout`, `stderr`, `timed_out`, `termination_confirmed`, `effect_unknown`, `job_id`, ...) plus the readable `exit code: N` text; each stream is trimmed to its first and last 10,000 characters. At a foreground run's `timeout` (default 120 s) agentpc attempts to stop it and returns its partial output, and the reply says whether the stop was confirmed; `background: true` (servers, long jobs) returns a job id to poll with `get_job_status`. See "Running commands" |
| `get_job_status(name, id, tail_lines?, wait_seconds?, cursor?)` | Job state (queued, running, exited with its code, stopped, ended without an exit code, or missing) plus its log, typed. `wait_seconds` (0 by default, at most 30) waits without ever stopping the job; `cursor` reads the log incrementally. Also takes a foreground run's `job_id`. See "Watching jobs" |
| `stop_job(name, id)` | Stop a background job (or a foreground run's `job_id`) and everything it started; for an exited job, only what it left running |
| `upload_file` / `download_file` | Copy files or folders between this Mac and a VM |
| `forward_port(name, guest_port, host_port?, protocol?)` | Reach a server in the VM from the Mac at `127.0.0.1:<host_port>` (a free port if omitted). TCP (default) is an SSH tunnel: reaches a server on the guest's own `127.0.0.1`. `protocol: "udp"` is a QEMU host forward: the guest server must listen on `0.0.0.0`; not on offline VMs. Lasts until the VM stops |
| `list_forwards(name)` / `delete_forward(name, host_port, protocol?)` | List a VM's forwards (with protocol) / stop one |
| `read_vm_log(name, which, tail_lines?)` | Tail a VM's `qemu` or `serial` log when it won't boot or the desktop is unreachable |
| `list_desktop_tools(name, tool?)` | Desktop-control tools in that VM with their required arguments and read-only marks, or one tool's full schema with its annotations |
| `use_desktop_tool(name, tool, arguments?)` | Call a GUI tool: click, type, launch apps, read the UI tree. A wrong name returns close matches. Marked destructive |
| `inspect_desktop(name, tool, arguments?)` | Read-only twin of `use_desktop_tool` (same arguments): runs a tool only if the VM's driver explicitly marks it read-only, and refuses anything else before dispatching it. See "Looking without acting" |

## How to work

1. `list_vms` to see images and existing VMs, then `create_vm` your own VM for the task with a
   name that identifies it (e.g. the task or your agent name). Don't reuse a VM you didn't create.
2. Prefer `run_command` for anything a shell can do. It's faster and more reliable than the GUI.
   Use `upload_file` to bring in what you need to test (an installer, a script, a build).
3. Before a risky or slow-to-redo step (an installer, a config change), `checkpoint_vm` so
   `restore_vm` can undo it in seconds instead of rebuilding from `reset_vm`. Port forwards
   (TCP and UDP) must be set up again after a restore.
4. For GUI work, loop: look (`take_screenshot`, or `inspect_desktop` with a UI-snapshot tool), act
   (`use_desktop_tool`), then look again to verify.
5. Call `list_desktop_tools` once per VM to learn the exact tool names and required arguments.
   A call with wrong arguments returns that tool's argument list; a wrong tool name returns
   close matches ("did you mean ..."). The desktop driver is pinned per image so tools match
   these docs: don't update it inside a VM (`reset_vm` restores it). If a reply starts with a
   reconnect note (the VM or its driver restarted), earlier snapshot ids and browser sessions
   are gone: take a new snapshot and run `browser_prepare` again.
6. When finished, `delete_vm` VMs you created, unless the user wants to keep them.

## Running commands

`run_command` returns `structuredContent` (its `outputSchema`, `schema_version` 1) and one text block
holding the readable `exit code: N` / `--- stdout ---` / `--- stderr ---` text. The text is not a JSON
copy, so a client that joins text blocks sees the same text as ever. Read the typed fields and trust
`isError`:

- `state`: `exited` (`exit_code` is its code), `timed_out`, `not_started` (it never ran),
  `connection_lost` (SSH failed while it may have been running) or `started` (a background job).
- `stdout`, `stderr`, `stdout_bytes`, `stderr_bytes`, `stdout_truncated`, `stderr_truncated`,
  `output_truncated`; `timed_out`, `termination_confirmed`, `effect_unknown`, `recovery_action`, `error`;
  `execution_id` (issued to each run; null only when the call failed before one was, such as an unknown or stopped VM), `job_id` (background jobs, and foreground runs whose fate is
  uncertain) and `log_path`.
- `isError` is true for any nonzero exit, any timeout (even when the command's own code is known to be
  0), and any run that did not start or lost its connection. It is false only for exit 0 and a
  background start. Foreground exit codes are the low 8 bits SSH reports (Windows `exit -1` reads 255).
- At `timeout`, agentpc attempts to stop the command and what it started, and keeps the partial output.
  `termination_confirmed: true` means it verified that none of the processes it identified is left: on
  Linux the command's transient systemd user scope when the guest's user manager is available, the
  processes marked with its execution id and their descendants (all that is checked without a user
  manager, where a process that clears its environment and detaches can be missed), and its verified
  root; on Windows its verified process tree (best effort: a process whose launcher had exited can be
  missed). It is not a sandbox guarantee. System services (`systemctl`), other systemd units, and Windows
  services, scheduled tasks and WMI-created processes are not covered. With
  `termination_confirmed: false` or `effect_unknown: true` (`connection_lost` with `effect_unknown: true`
  included) it may still be running: use `job_id` with `get_job_status` / `stop_job` and don't rerun it
  before you know. A `connection_lost` with `effect_unknown: false` carries the VM's recorded `exit_code`
  (it finished; its output may be incomplete) and a null `job_id`. `retry` in
  `recovery_action` means it provably never ran; agentpc never retries a command itself.
- After a timeout the reply usually follows within a few seconds, and at most about 50 seconds later
  while agentpc finishes its bounded cleanup.
- A command whose exit code the VM recorded but whose run had not ended by the timeout (a child it left
  may have held its output, or it ended right at the deadline) returns that exit code with
  `timed_out: true`, after cleanup verified that none of the processes agentpc identified remains: start
  servers with `background: true`.
- On Linux a foreground command is limited to 92 KiB (94,208 UTF-8 bytes) of text (base64 in one SSH
  command line, and an environment string, each capped at 128 KiB by the kernel). agentpc checks it
  before sending: a bigger one is `not_started` with no `execution_id` and no `recovery_action`. For a
  bigger script, `upload_file` it and run the file.

## Watching jobs

`get_job_status` also returns typed `structuredContent` (`state`: `queued`, `running`, `exited`,
`stopped`, `ended`, `not_started`, `missing`, `unknown`; `exit_code`, `kind`, `stop_reason`, `log_tail`
or `output`, `has_more`, `next_cursor`, `cursor_reset`, `cursor_reset_reason`, `wait_timed_out`, `note`,
`error_code`) next to its `STATE: ...` text.

- `get_job_status {name, id}` answers at once with the last `tail_lines` (default 50) lines, at most
  16 KiB, as before, plus a `next_cursor` at the end of what it read.
- `wait_seconds` (0 to 30, default 0) waits for the job to leave `running`, or with a `cursor` for new
  output, then returns the current state with `wait_timed_out: true` if nothing happened. A wait only
  waits: it never stops the job. For longer waits call again with the last `next_cursor`.
- `cursor` (a `next_cursor`, or `"start"`) reads the log from that point, at most 16 KiB a reply; the
  next cursor advances by exactly the bytes returned, so reads neither repeat nor skip output (`has_more`
  says call again). It is opaque. UTF-8 characters are never split across reads; invalid bytes become
  U+FFFD with `invalid_utf8: true`; a byte-order mark is consumed unseen.
- A cursor that no longer matches (VM reset or restored, guest rebooted, log replaced, shortened,
  rewritten or removed) comes back with `cursor_reset: true` and a `cursor_reset_reason`, restarted at
  the log's beginning: discard what you assumed. Detection is best effort (it can miss a guest clock
  step, a Windows log recreated within NTFS tunneling, or a log rewritten with the same first 64 bytes).
- `missing` means nothing has recorded itself under that id yet. That does not prove the command never
  started: don't blindly retry. An unreachable VM is an error (`state: unknown`), never `ended`.
- An exited job that left a child running still reports its own exit code. A foreground `job_id` has
  status only (exited with its code, stopped at its timeout, or not started), no log to replay.

## Looking without acting

`inspect_desktop` takes `use_desktop_tool`'s arguments but is annotated read-only (not destructive,
open-world: results can include web page text). It asks the VM's driver for the tool's annotations
before each dispatch and runs only a tool the driver explicitly marks `readOnlyHint: true`
(`list_desktop_tools` shows `[read-only]`); a tool marked otherwise, unmarked or unknown is refused
before it is dispatched (looking up the annotations does ask the VM's driver). After a dropped call the driver is asked again on the new connection, and the
call is re-sent only if the old connection had listed the tool read-only and the new one still does. The
marking is the driver's own claim: a guard against mistakes, not against a mislabelling driver.
Compact observation: `get_window_state` with `include_screenshot: false` returns the element tree
without a screenshot, and its `query` argument narrows it (`list_desktop_tools` with `tool` shows the
pinned driver's schema). Tools that act still go through `use_desktop_tool`.

## Windows (desktop tools from cua-driver)

- `launch_app` takes a name such as `notepad` and returns the `pid` and `window_id`s. It
  launches without stealing focus.
- `get_window_state(pid, window_id)` returns numbered elements and a `snapshot_id`;
  `click` and `type_text` take that `snapshot_id` with an `element_index`.
- `get_desktop_state` returns a screenshot of the whole screen.
- Input goes to the app in the background. Keys typed into whatever has focus (e.g. after
  `ctrl+l`), scroll, drag and right-click often need `"delivery_mode": "foreground"`; a reply
  that says "not verified" or "retry with foreground" means use it.
- If `get_window_state` comes back "degraded" with no elements (modern Notepad does this), act
  by pixels: pass `x`/`y` read from the screenshot of a `get_window_state` call that included
  one (the default). Also check for another window of the same app (a pop-up or dialog). Browser
  page content appears a few seconds after load; read again if it's missing.
- Modern apps (Notepad, Settings) ignore raw shortcuts like `ctrl+s`: use `invoke_menu` with a
  path such as `["File", "Save as"]`, or click the menu items.
- Dialogs, menus and pop-ups are separate windows: `list_windows` again to find them.
  Classic file dialogs don't take `set_value`: click the "File name" field, then `type_text`
  in the foreground. Several buttons can share a name (e.g. "Open"); pick the plain button,
  not the dropdown.
- The taskbar, Start menu and desktop aren't listed windows: use `click` with
  `"scope": "desktop"` and screen coordinates (`"button": "right"` for the desktop menu).
  `foreground_unavailable` can come back even when the click worked; check with a screenshot.
- `run_command` runs PowerShell as the `agent` administrator. The screen is 1280x800.
- Processes started over `run_command` end when the command returns: start servers and GUI apps
  with `background: true`. `forward_port` tunnels over SSH, so it reaches a server on the guest's
  own `127.0.0.1` with no Windows firewall change; a `protocol: "udp"` forward reaches the
  guest's network address, so its server must listen on `0.0.0.0` and be allowed through the
  firewall (`New-NetFirewallRule -Direction Inbound -Protocol UDP -LocalPort <port> -Action Allow`).
- SmartScreen, Windows Update and first-run pop-ups are turned off. Edge is the browser.
- It's a clean install: no Visual C++ redistributable, no .NET (only .NET Framework 4.8.1),
  no PowerShell 7. A missing `VCRUNTIME140.dll` means the app under test doesn't ship its
  runtime; report that rather than installing it, unless the user asks.

## Ubuntu (desktop tools from cua-driver, XFCE on X11)

- `get_desktop_state` returns a screenshot plus the `pid` and `window_id` values other
  tools need.
- Keyboard and mouse tools need `"delivery_mode": "foreground"`.
- `launch_app` takes a command name such as `xfce4-terminal`.
- `run_command` runs bash as `agent`, with passwordless `sudo`. The screen is 1280x800.
- Opening a `.deb` (a double-click, or `xdg-open`) installs it with apt in a terminal window
  that closes on success and stays open on a failure.
- Google Chrome is installed. To read a page: `launch_app` with `name: "google-chrome"` and
  the URL in `additional_arguments`, then `get_window_state` on its window; the page's text,
  links and fields are in the tree.
- To drive a page with the browser tools: `browser_prepare` with `allow_launch: true` and
  `profile: {"mode": "isolated_new"}`, then `list_windows` with its `prepared_pid` for
  Chrome's `window_id`, `get_browser_state` with those to get `target_id`/`tab_id`, then
  `browser_navigate`, `browser_click`, `browser_type`. Pass the same `session` label on every
  call. A session ends after about 5 minutes without calls, or if the driver connection drops
  (the reply then says so): run `browser_prepare` again. `launch_app` with `urls` returns no
  pid; don't use the legacy `page` tool.

## Arch (Arch Linux ARM: like Ubuntu, with pacman and Chromium)

- Arch Linux ARM is a community port of Arch. The desktop, `run_command` and desktop tools
  work as on Ubuntu.
- The browser is Chromium: use `name: "chromium"` wherever Ubuntu uses `"google-chrome"`;
  the browser tools work the same.
- Install packages with `sudo pacman -Syu --noconfirm <pkg>`: Arch doesn't support partial
  upgrades, and an image's package lists age. The first `-Syu` may upgrade the whole system:
  give `run_command` a longer `timeout` or `background: true`. After a kernel upgrade, reboot
  with `start_vm` before loading new modules.
- `/tmp` is a tmpfs, cleared at every boot (unlike Ubuntu's).

## Guest tips

- **Reaching the Mac / other VMs.** From a guest, `10.0.2.2` is the Mac host (a Mac dev server
  on `127.0.0.1` or `0.0.0.0` is reachable there). VMs can't reach each other directly: to let VM
  A hit a server in VM B, `forward_port(B, guest_port, host_port)`, then from A connect to
  `10.0.2.2:<host_port>`; for UDP pass `protocol: "udp"` and have B's server listen on `0.0.0.0`
  (a Windows guest also needs a firewall rule for it). An `offline: true` VM can't reach
  `10.0.2.2`; its forwarded TCP ports still work (UDP forwards don't). Every VM's own address
  is `10.0.2.15`, so a service that advertises its address advertises the same one from each
  VM; peers elsewhere reach it only through a forward.
- **Reboots.** A reboot (Windows Update, some installers) drops SSH. Call `start_vm` on the same
  VM — it waits until the desktop is ready again — then rerun the command. If its reply was
  `connection_lost` with `effect_unknown: true`, check it first with `get_job_status` and its `job_id`.
- **GUI installers** return immediately; run them silently and wait:
  `Start-Process installer.exe -ArgumentList '/S' -Wait -PassThru` (check `.ExitCode`).
- **Windows Update is disabled** in the image, which breaks DISM/`Add-WindowsCapability` optional
  features (.NET 3.5, RSAT, language packs). Re-enable temporarily:
  `Set-Service wuauserv -StartupType Manual; Start-Service wuauserv`, then set it back to `Disabled`.
- **Defender real-time protection is on** (only SmartScreen is disabled) and may quarantine freshly
  built or unsigned test binaries. Exclude a path: `Add-MpPreference -ExclusionPath C:\work`
  (Tamper Protection can block `Set-MpPreference -DisableRealtimeMonitoring $true`).
- **x64 apps run, slower.** Windows is ARM64; x64 and x86 programs (installers, desktop apps,
  CLIs) run through Windows' built-in Prism emulation, roughly 2–4× slower than native. x64
  drivers, kernel-mode software and anti-cheat don't. Prefer an ARM64 build when one exists.
- **Proxy / corporate CA.** The guest inherits no Mac proxy; set `HTTP(S)_PROXY` inside it and
  import a corporate root with `Import-Certificate` (Windows), `update-ca-certificates` (Ubuntu)
  or `sudo trust anchor --store <cert>` (Arch).
  Mac VPNs apply automatically (the VM's NAT rides the Mac's network).
- **No GPU acceleration** (2D virtio GPU: WebGL is software or off), **no audio device**, fixed
  1280x800.

## Rules

- Act only on VMs you created. Never `reset_vm`, `stop_vm`, `restore_vm` or `delete_vm` a VM
  you didn't create unless the user asks. VMs are resettable: `reset_vm` your own broken one
  instead of repairing it, and `delete_vm` it when done.
- Open the Mac’s browser only when the user asks to watch: `create_vm` with
  `open_in_browser: true`. Omit it otherwise; the viewer is still available. It is a per-call
  action, so a retry with it set can open another tab.
- To test untrusted software or offline behaviour, `create_vm` with `offline: true`: no internet
  and no access to the Mac, while `run_command`, files, desktop tools and `forward_port` work.
- For a heavy build, `create_vm` accepts `memory_gb` and `cpus`; a non-default size boots
  cold (~25 s Windows, ~15 s Ubuntu and Arch) instead of resuming in seconds.
- Output that needs more than 10,000 characters from each end (`output_truncated`): write it to a
  file in the VM and `download_file` it.
- Don't create VMs you won't use. Each running VM uses 4 GB (Ubuntu, Arch) or 8 GB (Windows) of RAM.
- The first `create_vm ubuntu` downloads the Ubuntu image (~1.2 GB). Another Ubuntu release
  (`version: "22.04"`, `"26.04"`, ...) is fetched or built the same way (~3 min). A dated
  version (`"24.04-YYYYMMDD"`) pins one published build: download-only, never built.
- `create_vm arch` (rolling release; no version needed, `x86apps`, or `rolling-YYYYMMDD` /
  `rolling-x86apps-YYYYMMDD` to pin a published build)
  downloads the Arch image on first use, or builds it locally (~6 min) if the download fails.
- **x86 Linux programs** (x86_64 and i386) need an x86apps VM: `create_vm(os: "ubuntu" or
  "arch", version: "x86apps")`. FEX translates them to arm64, about 2× slower (JIT runtimes
  like Node 6–7×); run them directly (`./tool`). Go programs work (the image sets
  `GODEBUG=asyncpreemptoff=1` for them). x86 containers work too: install Docker (`sudo apt
  install docker.io` on Ubuntu; `sudo pacman -Syu --noconfirm docker && sudo systemctl start
  docker` on Arch), then `docker run --platform linux/amd64 ...`. x86 Electron/Chromium apps
  (VS Code, Slack, ...) need `--no-sandbox`. A missing x86 library on Ubuntu: `sudo apt install
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
  `list_vms` shows `x86_tso`: `hardware` (fast; needs macOS 15+) or `emulated`. The first
  create downloads the image, or builds it locally (~8 min Ubuntu, ~10 min Arch) if the
  download fails.
- Windows images can't be downloaded. If `list_vms` shows no Windows image, ask the user to
  build one once (~12 min) and don't start it yourself:
  `agentpc image build windows` (downloads the official ISO from Microsoft, 7.3 GB). Other
  versions: `windows-11-24h2`, `windows-11-23h2`. Pass `version` (e.g. `"11-24h2"`) when the
  Windows release matters; without it you get 25H2, or the newest Windows 11 image installed.
- Don't put real credentials or secrets into a VM. The guest login is `agent` / `agent` (desktop
  and sudo; SSH takes only agentpc's key), and the VMs are reachable from anything on the Mac.
- VMs you create, start, reset or restore are stopped (never deleted) when the session ends,
  unless `AGENTPC_KEEP_RUNNING=1`. If a session's server is killed instead, the
  next agentpc MCP server to start stops them. `list_vms` shows `owner_running: false` for a VM
  whose session is gone; if you created it (a restarted session), `create_vm` with its name and
  image takes it back.
- If a tool's options look older than what agentpc does (an OS or image `list_vms` shows that
  `create_vm` doesn't list), your tool definitions predate an agentpc update: reconnect the
  agentpc MCP server or start a new session. `list_vms` says when the binary was replaced.
- An image in `list_vms` with `outdated` set was captured by an older agentpc, so it lacks newer
  guest fixes; tell the user the command it gives (don't run it yourself; it needs that image's
  VMs deleted).
