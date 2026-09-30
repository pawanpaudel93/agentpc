---
name: agentpc
description: Use agentpc to get an instant, resettable Windows or Ubuntu desktop VM on the user's Mac. Use it when a task needs a real, clean Windows or Linux machine, such as testing an installer, script or app on a fresh OS, reproducing a platform-specific bug, operating a GUI application, or taking screenshots of a desktop, and when the user mentions agentpc or asks for a Windows or Ubuntu VM.
---

# agentpc: instant, resettable desktops

agentpc runs Windows 11 and Ubuntu (24.04 by default, or another release)
VMs locally and exposes them through the `agentpc` MCP server. A new VM is ready in about 1 s
(Ubuntu) or 4 s (Windows) and can be reset to a clean install just as fast, so treat VMs as
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
| `list_vms` | VMs (state, size, checkpoints) and the available images with OS versions. Start here. |
| `create_vm(os, version?, name?, memory_gb?, cpus?, offline?)` | New VM (`ubuntu` or `windows`, optionally a version such as `22.04`); returns when the desktop is ready. `offline: true` cuts it off from the internet and this Mac |
| `start_vm` / `stop_vm` | Boot a stopped VM / shut one down |
| `reset_vm(name)` | Discard all changes: back to a clean install |
| `checkpoint_vm(name, label)` / `restore_vm(name, label)` / `delete_checkpoint(name, label)` | Save the VM's disk and memory; go back to that state in seconds; or drop one checkpoint |
| `delete_vm(name)` | Delete a VM with its disk and checkpoints |
| `take_screenshot(name, save_to?)` | PNG screenshot; works even while booting or hung. `save_to` also writes it to a Mac path |
| `run_command(name, command, timeout?, background?)` | Shell command: PowerShell on Windows, bash on Ubuntu. Returns `exit code: N` plus stdout and stderr, each trimmed to its first and last 10,000 characters. Foreground runs are killed at `timeout` (default 120 s) with partial output; `background: true` (servers, long jobs) returns a job id to poll with `get_job_status` |
| `get_job_status(name, id, tail_lines?)` | Background job's state (running, or exited with its code) plus its log tail |
| `upload_file` / `download_file` | Copy files or folders between this Mac and a VM |
| `forward_port(name, guest_port, host_port?)` | Reach a server in the VM from the Mac at `127.0.0.1:<host_port>` (a free port if omitted). SSH tunnel: reaches a server on the guest's own `127.0.0.1`; lasts until the VM stops |
| `list_forwards(name)` / `delete_forward(name, host_port)` | List a VM's forwards / stop one |
| `read_vm_log(name, which, tail_lines?)` | Tail a VM's `qemu` or `serial` log when it won't boot or the desktop is unreachable |
| `list_desktop_tools(name, tool?)` | List the GUI tools inside a VM, or one tool's full schema |
| `use_desktop_tool(name, tool, arguments?)` | Call a GUI tool: click, type, launch apps, read the UI tree |

## How to work

1. `list_vms` to see images and existing VMs, then `create_vm` your own VM for the task with a
   name that identifies it (e.g. the task or your agent name). Don't reuse a VM you didn't create.
2. Prefer `run_command` for anything a shell can do. It's faster and more reliable than the GUI.
   Use `upload_file` to bring in what you need to test (an installer, a script, a build).
3. Before a risky or slow-to-redo step (an installer, a config change), `checkpoint_vm` so
   `restore_vm` can undo it in seconds instead of rebuilding from `reset_vm`. Port forwards
   must be set up again after a restore.
4. For GUI work, loop: look (`take_screenshot` or a UI-snapshot tool), act (`use_desktop_tool`), then
   look again to verify.
5. Call `list_desktop_tools` once per VM to learn the exact tool names and required arguments.
   A call with wrong arguments returns that tool's argument list. The desktop driver is pinned
   per image so tools match these docs: don't update it inside a VM (`reset_vm` restores it).
6. When finished, `delete_vm` VMs you created, unless the user wants to keep them.

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
  own `127.0.0.1` with no Windows firewall change.
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

## Guest tips

- **Reaching the Mac / other VMs.** From a guest, `10.0.2.2` is the Mac host (a Mac dev server
  on `127.0.0.1` or `0.0.0.0` is reachable there). VMs can't reach each other directly: to let VM
  A hit a server in VM B, `forward_port(B, guest_port, host_port)`, then from A connect to
  `10.0.2.2:<host_port>`. An `offline: true` VM can't reach `10.0.2.2`; its forwarded ports still work.
- **Reboots.** A reboot (Windows Update, some installers) drops SSH. Call `start_vm` on the same
  VM — it waits until the desktop is ready again — or just retry `run_command`.
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
  import a corporate root with `Import-Certificate` (Windows) or `update-ca-certificates` (Ubuntu).
  Mac VPNs apply automatically (the VM's NAT rides the Mac's network).
- **No GPU acceleration** (2D virtio GPU: WebGL is software or off), **no audio device**, fixed
  1280x800.

## Rules

- Act only on VMs you created. Never `reset_vm`, `stop_vm`, `restore_vm` or `delete_vm` a VM
  you didn't create unless the user asks. VMs are resettable: `reset_vm` your own broken one
  instead of repairing it, and `delete_vm` it when done.
- To test untrusted software or offline behaviour, `create_vm` with `offline: true`: no internet
  and no access to the Mac, while `run_command`, files, desktop tools and `forward_port` work.
- For a heavy build, `create_vm` accepts `memory_gb` and `cpus`; a non-default size boots
  cold (~25 s Windows, ~15 s Ubuntu) instead of resuming in seconds.
- Output that needs more than 10,000 characters from each end: write it to a file in the VM
  and `download_file` it.
- Don't create VMs you won't use. Each running VM uses 4 GB (Ubuntu) or 8 GB (Windows) of RAM.
- The first `create_vm ubuntu` downloads the Ubuntu image (~1.2 GB). Another Ubuntu release
  (`version: "22.04"`, `"26.04"`, ...) is fetched or built the same way (~3 min).
- **x86_64 Linux programs** need `create_vm(os: "ubuntu", version: "x86apps")`: FEX translates
  them to arm64, about 2x slower (JIT runtimes like Node ~6x); run them directly (`./tool`).
  Go programs work (the image sets `GODEBUG=asyncpreemptoff=1` for them). A missing x86
  library: `sudo apt install libfoo:amd64`. x86 containers need Docker's qemu emulator
  (`docker run --privileged --rm tonistiigi/binfmt --install amd64`), which then handles every
  x86 program in that VM, more slowly. x86 Electron/Chromium apps (VS Code, Slack, ...) need
  `--no-sandbox`. The first create builds the image locally.
- Windows images can't be downloaded. If `list_vms` shows no Windows image, ask the user to
  build one once (~12 min) and don't start it yourself:
  `agentpc image build windows` (downloads the official ISO from Microsoft, 7.3 GB). Other
  versions: `windows-11-24h2`, `windows-11-23h2`. Pass `version` (e.g. `"11-24h2"`) when the
  Windows release matters; without it you get 25H2, or the newest Windows 11 image installed.
- Don't put real credentials or secrets into a VM. The guest login is `agent` / `agent`, and
  the VMs are reachable from anything on the Mac.
- VMs you create or start are stopped (never deleted) when the session ends, unless
  `AGENTPC_KEEP_RUNNING=1`.
