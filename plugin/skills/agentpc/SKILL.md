---
name: agentpc
description: Use agentpc to get a disposable Windows 11 or Ubuntu desktop VM on the user's Mac. Use it when a task needs a real, clean Windows or Linux machine, such as testing an installer, script or app on a fresh OS, reproducing a platform-specific bug, operating a GUI application, or taking screenshots of a desktop, and when the user mentions agentpc or asks for a Windows or Ubuntu VM.
---

# agentpc: disposable desktops

agentpc runs Windows 11 and Ubuntu 24.04 VMs locally and exposes them through the `agentpc`
MCP server. A new VM is ready in about 1 s (Ubuntu) or 4 s (Windows) and can be reset to a
clean install just as fast, so treat VMs as throwaway sandboxes.

## If the tools are missing

If no `vm_*` tools are available, the `agentpc` binary isn't installed or isn't on `PATH`.
Tell the user to install it (Apple Silicon Mac only), then restart the session:

```sh
curl -fsSL https://raw.githubusercontent.com/pawanpaudel93/agentpc/main/install.sh | sh
```

## Tools

| Tool | Use |
| --- | --- |
| `vm_list` | VMs, their state, and the available images with OS versions. Start here. |
| `vm_create(os, name?)` | New VM (`ubuntu` or `windows`); returns when the desktop is ready |
| `vm_start` / `vm_stop` | Boot a stopped VM / shut one down |
| `vm_reset(name)` | Discard all changes: back to a clean install |
| `vm_delete(name)` | Delete a VM |
| `vm_screenshot(name)` | PNG screenshot; works even while booting or hung |
| `vm_exec(name, command)` | Shell command: PowerShell on Windows, bash on Ubuntu |
| `desktop_tools(name, tool?)` | List the GUI tools inside a VM, or one tool's full schema |
| `desktop(name, tool, arguments)` | Call a GUI tool: click, type, launch apps, read the UI tree |

## How to work

1. `vm_list`; reuse a running VM of the right OS, or `vm_create` one.
2. Prefer `vm_exec` for anything a shell can do. It's faster and more reliable than the GUI.
3. For GUI work, loop: look (`vm_screenshot` or a UI-snapshot tool), act (`desktop`), then
   look again to verify.
4. Call `desktop_tools` once per VM to learn the exact tool names and arguments.
5. When finished, `vm_delete` VMs you created, unless the user wants to keep them.

## Windows (desktop tools from Windows-MCP)

- Call `Snapshot` first to get element labels and coordinates.
- `Click` takes `loc: [x, y]`; `Type` needs `loc` or `label`.
- `App` with `mode: "launch"` opens programs by name; `Shortcut` sends key combinations.
- `vm_exec` runs PowerShell as the `agent` administrator.

## Ubuntu (desktop tools from cua-driver, XFCE on X11)

- `get_desktop_state` returns a screenshot plus the `pid` and `window_id` values other
  tools need.
- Keyboard and mouse tools need `"delivery_mode": "foreground"`.
- `launch_app` takes a command name such as `xfce4-terminal`.
- `vm_exec` runs bash as `agent`, with passwordless `sudo`.

## Rules

- VMs are disposable: `vm_reset` a broken one instead of repairing it.
- Don't create VMs you won't use. Each running VM uses 4 GB (Ubuntu) or 8 GB (Windows) of RAM.
- The first `vm_create ubuntu` downloads the Ubuntu image (~1.2 GB).
- Windows images can't be downloaded. If `vm_list` shows no Windows image, ask the user to
  build one once (~12 min) and don't start it yourself:
  `agentpc image build windows --iso <Windows 11 ARM64 ISO>`.
- Don't put real credentials or secrets into a VM. The guest login is `agent` / `agent`, and
  the VMs are reachable from anything on the Mac.
