# agentpc

Disposable Windows 11 and Ubuntu desktop VMs on an Apple Silicon Mac, controllable by AI
coding agents (Claude Code, Codex, Gemini CLI, Cursor, VS Code, or any MCP client).
One Rust binary, `agentpc`: a CLI and, via `agentpc mcp`, an MCP stdio server.
Native QEMU + HVF; no manual steps after the one-time bake.

## Install

```sh
curl -fsSL https://raw.githubusercontent.com/pawanpaudel93/agentpc/main/install.sh | sh
```

The installer verifies the release checksum, puts `agentpc` in `~/.local/bin` (no sudo),
adds it to your PATH, installs QEMU with Homebrew if missing (installing Homebrew first if
needed; that asks for your password once), registers the MCP server with your agents and
runs `agentpc doctor`. Re-run it to upgrade.

Options: `AGENTPC_VERSION=0.1.0` pins a version, `AGENTPC_INSTALL_DIR=...` changes the
install dir, `AGENTPC_NO_MCP=1` skips MCP registration.

**Prerequisites:** an Apple Silicon Mac (M1 or later), macOS 14+ recommended, and QEMU from
Homebrew (handled by the installer).

## Quick start

```sh
agentpc bake ubuntu        # once, ~3 min: builds the golden image
agentpc new ubuntu         # a fresh desktop in ~1 s
```

Then ask your agent things like *"open a terminal on ubuntu-1 and run uname -a"* or
*"reset ubuntu-1 and check that my install script works on a clean machine"*.

Windows needs a Windows 11 ARM64 ISO from Microsoft, and the first bake also needs
colima + docker (`brew install colima docker`) to build the setup disk:

```sh
agentpc bake windows --iso ~/Downloads/<file>.iso   # once, ~12 min
agentpc new windows                                  # ~4 s
```

Without `--iso`, bake uses `$WIN_ISO` or `~/Downloads/*A64FRE*.iso`.

## MCP server

One server, `agentpc`, for all instances. `agentpc mcp-install` registers it with Claude
Code, Codex, Cursor, Gemini CLI and VS Code (user scope). For any other client:

```json
{ "mcpServers": { "agentpc": { "command": "agentpc", "args": ["mcp"] } } }
```

This repo also ships project configs (`.mcp.json`, `.codex/config.toml`,
`.gemini/settings.json`, `.cursor/mcp.json`, `.vscode/mcp.json`) that launch the installed
binary.

| Tool | Use |
|------|-----|
| `vm_list` | Instances, their state, and which golden images exist |
| `vm_create`, `vm_start`, `vm_stop`, `vm_reset`, `vm_delete` | Lifecycle; `vm_reset` = back to a clean install |
| `vm_screenshot` | PNG straight from the hypervisor, works while booting or hung |
| `vm_exec` | Shell over SSH: PowerShell on Windows, bash on Ubuntu |
| `desktop_tools`, `desktop` | List / call the desktop-control tools inside the VM |

| Guest   | Desktop                   | Desktop-control server |
|---------|---------------------------|------------------------|
| windows | Windows 11 ARM            | [Windows-MCP](https://github.com/CursorTouch/Windows-MCP) |
| ubuntu  | Ubuntu 24.04, XFCE on X11 | [cua-driver](https://github.com/trycua/cua) over SSH |

See [AGENTS.md](AGENTS.md) for tool usage tips.

## CLI

```sh
agentpc bake ubuntu|windows [--iso PATH]   # install an OS once into a golden image
agentpc snapshot ubuntu|windows            # recapture the live snapshot (bake does this)
agentpc new ubuntu|windows [name]          # clone + resume (default name <os>-<n>)
agentpc list                               # instances and golden images
agentpc info ubuntu-1                      # viewer URL, SSH, VNC
agentpc start|stop|reset|rm <name>         # reset = back to the golden state
agentpc ssh windows-1 'Get-Process'        # no command = interactive shell
agentpc screen ubuntu-1 [out.png]          # screenshot
agentpc mcp                                # MCP server on stdio (what agents launch)
agentpc mcp-install [clients...]           # register with claude, codex, cursor, gemini, vscode
agentpc doctor                             # check prerequisites
```

State lives in `~/.agentpc` (override with `AGENTPC_HOME`). Every instance has a browser
viewer (noVNC) on the shared viewer at `http://127.0.0.1:8100`; `agentpc info` prints the
URL for each instance.

## How it works

- **Golden images + copy-on-write clones.** `bake` installs a guest into a read-only golden
  image, then boots it once and saves its RAM with the desktop already running (the live
  snapshot; `agentpc snapshot <os>` recreates it). `new` creates a qcow2 overlay on it and
  resumes that RAM, so a clone costs a few MB and is ready in ~1 s (Ubuntu) or ~4 s (Windows)
  instead of booting (~14 s / ~25 s). Later `start`s after a `stop` are cold boots; `reset`
  resumes a fresh copy again. Rebaking would break existing clones, so `bake` refuses while
  any exist.
- **Screenshots** come straight from QEMU as PNG in ~40 ms, independent of the guest.
- **Windows:** dockur/windows-arm builds `setup.img` once (answer file + ARM virtio
  drivers); the OEM script installs Windows-MCP at first logon.
- **Ubuntu:** the cloud image + cloud-init installs XFCE on X11 and cua-driver; cloud-init
  is disabled after the bake so clones don't re-provision.
- **Why not Docker?** dockur can't boot Windows on a Mac: Apple's vz gives nested KVM no
  PMU, and Windows ARM hangs. agentpc runs QEMU natively with HVF instead.

## Security

Everything binds to 127.0.0.1. The desktop-control servers inside the guests are
unauthenticated, but reachable only from this Mac. The guest login is `agent` / `agent`.
Treat instances as disposable sandboxes, not as security boundaries for secrets.

## Build from source

```sh
cargo build --release          # target/release/agentpc
```

Guest assets under `guests/` are embedded in the binary. MIT licensed.

## Releasing (maintainers)

1. Bump `version` in `Cargo.toml`, commit, then tag and push `vX.Y.Z`. The Release workflow
   builds the tarball, the MCP bundle (`agentpc-X.Y.Z.mcpb`), their `.sha256` files, and a
   filled-in `server.json`, and attaches them all to the GitHub Release.
2. Publish to the MCP Registry: download that release's `server.json` over the repo copy,
   then `brew install mcp-publisher`, `mcp-publisher login github`, `mcp-publisher publish`.
