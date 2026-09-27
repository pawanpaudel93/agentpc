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
curl -fsSL https://raw.githubusercontent.com/pawanpaudel93/agentpc/main/install.sh | sh
```

## Tools

| Tool | Use |
| --- | --- |
| `list_vms` | VMs (state, size, checkpoints) and the available images with OS versions. Start here. |
| `create_vm(os, version?, name?, memory_gb?, cpus?, offline?)` | New VM (`ubuntu` or `windows`, optionally a version such as `22.04`); returns when the desktop is ready. `offline: true` cuts it off from the internet and this Mac |
| `start_vm` / `stop_vm` | Boot a stopped VM / shut one down |
| `reset_vm(name)` | Discard all changes: back to a clean install |
| `checkpoint_vm(name, label)` / `restore_vm(name, label)` | Save the VM's disk and memory; go back to exactly that state in seconds |
| `delete_vm(name)` | Delete a VM with its disk and checkpoints |
| `take_screenshot(name)` | PNG screenshot; works even while booting or hung |
| `run_command(name, command, background?)` | Shell command: PowerShell on Windows, bash on Ubuntu. Returns `exit code: N` plus stdout and stderr; each is trimmed to its first and last 10,000 characters. `background: true` keeps it running after the call (servers, long jobs) and says where its output goes |
| `upload_file` / `download_file` | Copy files or folders between this Mac and a VM |
| `forward_port(name, guest_port)` | Reach a server running in the VM at `127.0.0.1:<port>` on the Mac |
| `list_desktop_tools(name, tool?)` | List the GUI tools inside a VM, or one tool's full schema |
| `use_desktop_tool(name, tool, arguments)` | Call a GUI tool: click, type, launch apps, read the UI tree |

## How to work

1. `list_vms`; reuse a running VM of the right OS and version, or `create_vm` one.
2. Prefer `run_command` for anything a shell can do. It's faster and more reliable than the GUI.
   Use `upload_file` to bring in what you need to test (an installer, a script, a build).
3. Before a risky or slow-to-redo step (an installer, a config change), `checkpoint_vm` so
   `restore_vm` can undo it in seconds instead of rebuilding from `reset_vm`. Port forwards
   must be set up again after a restore.
4. For GUI work, loop: look (`take_screenshot` or a UI-snapshot tool), act (`use_desktop_tool`), then
   look again to verify.
5. Call `list_desktop_tools` once per VM to learn the exact tool names and arguments.
6. When finished, `delete_vm` VMs you created, unless the user wants to keep them.

## Windows (desktop tools from Windows-MCP)

- Call `Snapshot` first to get element labels and coordinates.
- `Click` takes `loc: [x, y]`; `Type` needs `loc` or `label`.
- `App` with `mode: "launch"` opens programs by name; `Shortcut` sends key combinations.
- `run_command` runs PowerShell as the `agent` administrator. The screen is 1280x800.
- Processes started over `run_command` end when the command returns: start servers and GUI apps
  with `background: true`, and open the Windows firewall for ports you `forward_port`.
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
- Google Chrome is installed. Browser tools: `browser_prepare` with `allow_launch: true` and
  `profile: {"mode": "isolated_new"}`, then `list_windows` for Chrome's `pid`/`window_id`,
  `get_browser_state` with those to get `target_id`/`tab_id`, then `browser_navigate`,
  `browser_click`, `browser_type`. Pass the same `session` label on every call.

## Rules

- VMs are disposable: `reset_vm` a broken one instead of repairing it.
- To test untrusted software or offline behaviour, `create_vm` with `offline: true`: no internet
  and no access to the Mac, while `run_command`, files, desktop tools and `forward_port` work.
- For a heavy build, `create_vm` accepts `memory_gb` and `cpus`; a non-default size boots
  cold (~25 s Windows, ~15 s Ubuntu) instead of resuming in seconds.
- Output that needs more than 10,000 characters from each end: write it to a file in the VM
  and `download_file` it.
- Don't create VMs you won't use. Each running VM uses 4 GB (Ubuntu) or 8 GB (Windows) of RAM.
- The first `create_vm ubuntu` downloads the Ubuntu image (~1.2 GB). Another Ubuntu release
  (`version: "22.04"`, `"26.04"`, ...) is fetched or built the same way (~3 min).
- Windows images can't be downloaded. If `list_vms` shows no Windows image, ask the user to
  build one once (~12 min) and don't start it yourself:
  `agentpc image build windows` (downloads the official ISO from Microsoft, 7.3 GB). Other
  versions: `windows-11-24h2`, `windows-11-23h2`. Pass `version` (e.g. `"11-24h2"`) when the
  Windows release matters; without it you get 25H2, or the newest Windows 11 image installed.
- Don't put real credentials or secrets into a VM. The guest login is `agent` / `agent`, and
  the VMs are reachable from anything on the Mac.
