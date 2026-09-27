# agentpc — instructions for coding agents

This repo runs instant, resettable Windows and Ubuntu desktop VMs on an Apple Silicon Mac and
exposes them to you through one MCP server, `agentpc` (`agentpc mcp`). It is
preconfigured for Claude Code (`.mcp.json`), Codex (`.codex/config.toml`), Gemini CLI
(`.gemini/settings.json`), Cursor (`.cursor/mcp.json`) and VS Code (`.vscode/mcp.json`),
all launching the installed `agentpc` binary. If the `agentpc` tools are missing, the user
can run `agentpc mcp-install` (or install first: see README.md).

## Using the VMs

| Tool | Use |
| --- | --- |
| `list_vms` | VMs (owner, state, size, checkpoints, viewer) and the images (with OS version) they come from. Start here. |
| `create_vm(os, version?, name?, memory_gb?, cpus?, offline?)` | New clone: ubuntu ~1 s, windows ~4 s (resumed from a snapshot). Returns when ready; retrying in the same session returns the VM it already made. `memory_gb` 2–64, `cpus` 1–16. `offline` cuts off the internet. |
| `start_vm` / `stop_vm` / `reset_vm` / `delete_vm` | Lifecycle. `reset_vm` = back to a clean install. |
| `checkpoint_vm(name, label)` / `restore_vm(name, label)` / `delete_checkpoint(name, label)` | Save disk + memory before a risky step; restore in seconds; or drop one checkpoint. |
| `take_screenshot(name, save_to?)` | Hypervisor screenshot; works even while booting or hung. `save_to` also writes the PNG to a Mac path. |
| `run_command(name, command, timeout?, background?)` | Shell over SSH: PowerShell on windows, bash on ubuntu. Returns exit code, stdout, stderr (long output trimmed). Foreground runs are killed at `timeout` (default 120 s) with partial output; `background: true` returns a job id you poll with `job_status`. |
| `job_status(name, id, tail_lines?)` | State of a background job (running, or exited with its code) plus its log tail. |
| `upload_file` / `download_file` | Copy files or folders between the Mac and a VM. |
| `forward_port(name, guest_port, host_port?)` | Reach a server in the VM from the Mac at `127.0.0.1:<host_port>` (a free port if omitted). SSH tunnel: reaches a server on the guest's own `127.0.0.1`; lasts until the VM stops. |
| `list_forwards(name)` / `remove_forward(name, host_port)` | List a VM's forwards / stop one. |
| `read_vm_log(name, which, tail_lines?)` | Tail a VM's `qemu` or `serial` log when it won't boot or the desktop is unreachable. |
| `list_desktop_tools(name, tool?)` | Desktop-control tools in that VM, or one tool's full schema. |
| `use_desktop_tool(name, tool, arguments)` | Call one of those tools (click, type, launch, snapshot…). |

Work in a loop: look (`take_screenshot` or a snapshot tool) → act (`use_desktop_tool`) → look again to
verify. Prefer `run_command` for anything a shell can do; use `use_desktop_tool` for GUI-only work.

**Windows** (desktop tools from Windows-MCP): call `Snapshot` first for element labels and
coordinates. `Click` takes `loc: [x, y]`; `Type` needs `loc` or `label`; `App` with
`mode: "launch"` opens programs by name; `Shortcut` sends key combos.

**Ubuntu** (desktop tools from cua-driver, XFCE on X11): `get_desktop_state` returns a
screenshot plus window pid/window_id values to pass to other tools. Keyboard and mouse
tools need `"delivery_mode": "foreground"`. `launch_app` takes a command name such as
`xfce4-terminal`. Google Chrome is installed for the `browser_*` tools (see the plugin skill
for the call sequence).

Guest tips:

- **Networking.** VMs are isolated behind user-mode NAT. From a guest, `10.0.2.2` is the Mac
  host (a Mac server on `127.0.0.1`/`0.0.0.0` is reachable at `10.0.2.2:<port>`). For VM-to-VM,
  `forward_port(B, guest_port, host_port)` then connect from A to `10.0.2.2:<host_port>`. An
  offline VM can't reach `10.0.2.2` but forwarded ports still work. A guest inherits no Mac
  proxy; set `HTTP(S)_PROXY` and import a corporate CA in the guest itself.
- **Reboots** (Windows Update, some installers) drop SSH: call `start_vm` (it waits until the
  VM is ready again) or retry `run_command`.
- **Windows:** GUI installers return at once — `Start-Process x.exe -ArgumentList '/S' -Wait
  -PassThru` and check `ExitCode`. Windows Update is disabled (`wuauserv` off + `NoAutoUpdate`),
  which blocks DISM/`Add-WindowsCapability`; re-enable with `Set-Service wuauserv -StartupType
  Manual; Start-Service wuauserv`, then set it back to `Disabled`. Defender real-time protection
  is on (only SmartScreen is off) and may quarantine test binaries — `Add-MpPreference
  -ExclusionPath C:\work`.
- Guests are 1280x800 with a 2D-only GPU (no acceleration) and no audio device.

Rules:

- Instances are disposable; create your own uniquely named VM per task and `reset_vm` it
  instead of repairing a broken one. Don't reset, stop, restore or delete a VM you didn't
  create unless the user asks.
- Don't create instances you won't use, and `delete_vm` your instances when done.
  Each running VM takes 4 GB (ubuntu) or 8 GB (windows) of RAM.
- `create_vm ubuntu` downloads the Ubuntu image on first use (~1.2 GB); pass `version`
  (e.g. "22.04") for another release. A Windows image must be built by the user once:
  `agentpc image build windows` (downloads the ISO; ~12 min), or another version
  (`windows-11-24h2`, `windows-11-23h2`; `agentpc image build --help` lists them).
  If `list_vms` shows no windows image, ask the user to run that. Don't start a build
  yourself unless asked.
- The login for both guests is `agent` / `agent`. Everything binds to 127.0.0.1.
- VMs you create or start over MCP are stopped (never deleted) when the session ends, unless
  `AGENTPC_KEEP_RUNNING=1`.

## Working on this repo

- Commits follow Conventional Commits: `feat:`, `fix:`, `docs:`, `refactor:`, `test:`,
  `ci:` or `chore:`, then a short imperative subject; add a brief bullet body only if needed.
- Rust, single binary `agentpc` (CLI + MCP server). `src/main.rs` is the CLI; modules:
  `instance` (VMs, images, on-disk layout), `qemu`, `ops` (lifecycle),
  `viewer` (browser viewer), `image` (build/snapshot), `registry` (pull/push), `setup` (`doctor`, `mcp-install`, `clean`, `uninstall`), `mcp` (the server).
- Ports derive from the instance slot n: SSH 47000+n, Windows-MCP 47100+n, noVNC websocket
  47200+n, VNC 47300+n; the shared browser viewer is on 8100.
- Guest assets in `guests/` are embedded in the binary. A Windows build writes them to a
  FAT `setup.img`: `Autounattend.xml` drives Setup, `SetupComplete.cmd` runs after it, and
  `oem/setup.ps1` runs at first logon; `guests/ubuntu/user-data` is the Ubuntu cloud-init.
  `guests/<os>/prepare.*` runs in the guest every time a snapshot is captured (agent defaults:
  no pop-ups or updates, Chrome on Ubuntu), so it also upgrades existing and pulled images.
  Changes take effect on the next `agentpc image build`, which refuses while VMs of that image exist.
- Images are `<os>-<version>` (`Image` in `instance.rs`); a bare OS means its default version.
- State (images, keys, instances) lives in `~/.agentpc` (`AGENTPC_HOME` overrides).
- `plugin/` is the Claude plugin (MCP server + `skills/agentpc/SKILL.md`), listed by
  `.claude-plugin/marketplace.json`. Keep the skill's tool guidance in sync with the
  "Using the VMs" section above; check with `claude plugin validate plugin --strict`.
- Verify with `cargo clippy -- -D warnings` plus a real instance (`agentpc create ubuntu`, then
  the MCP tools). Unit tests can't cover the VM paths.
- Never commit `target/`.
