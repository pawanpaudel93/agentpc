# agentpc

[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)
![Platform: macOS on Apple Silicon](https://img.shields.io/badge/platform-macOS%20%7C%20Apple%20Silicon-lightgrey)
![MCP server](https://img.shields.io/badge/MCP-server-8A2BE2)

**Disposable Windows 11 and Ubuntu desktops for AI agents, on your Mac.**

agentpc gives AI agents (Claude Code, Claude Desktop, Codex, Cursor, Gemini CLI, VS Code, or
any MCP client) real desktop computers to work in: create a VM in about a second, let the agent click,
type, take screenshots and run commands, then reset it to a clean state. It is a single Rust
binary that runs VMs with QEMU on Apple's hypervisor and serves them to agents over MCP.

## Contents

- [Features](#features)
- [Requirements](#requirements)
- [Installation](#installation)
- [Quick start](#quick-start)
- [Using it with AI agents](#using-it-with-ai-agents)
- [CLI reference](#cli-reference)
- [Images](#images)
- [Configuration](#configuration)
- [How it works](#how-it-works)
- [Troubleshooting](#troubleshooting)
- [Security](#security)
- [Development](#development)
- [License](#license)

## Features

- **Instant VMs.** New VMs resume from a saved snapshot of a running desktop: ready in
  ~1 s (Ubuntu) or ~4 s (Windows). `reset` returns a VM to a clean state just as fast.
- **Real desktops.** Windows 11 Pro ARM and Ubuntu 24.04 (XFCE), each with a
  desktop-control server agents can drive: [Windows-MCP](https://github.com/CursorTouch/Windows-MCP)
  and [cua-driver](https://github.com/trycua/cua).
- **One MCP server for everything.** Agents create, drive, screenshot and delete VMs
  themselves. Works with any MCP client; `agentpc mcp-install` sets up the popular ones.
- **Shell and screen access.** Run PowerShell or bash over SSH; take PNG screenshots straight
  from the hypervisor in ~40 ms, even while a guest is booting or hung.
- **Watch along.** Every VM has a browser viewer, so you can see what the agent is doing.
- **Versioned images.** Each image records its OS version, source and build date; the
  Ubuntu image can be downloaded instead of built.
- **Local and private.** Everything runs on your Mac and listens on `127.0.0.1` only.

## Requirements

| Requirement | Details |
| --- | --- |
| Hardware | Apple Silicon Mac (M1 or later) |
| OS | macOS 14 or later (developed on macOS 15) |
| Runtime | [QEMU](https://www.qemu.org) from Homebrew (the installer handles it) |
| Memory | 4 GB per running Ubuntu VM, 8 GB per running Windows VM |
| Disk | ~8 GB for the Ubuntu image, ~30 GB for the Windows image (each including its snapshot) |
| Windows only | A Windows 11 ARM64 ISO from Microsoft; `colima` and `docker` for the first image build |

## Installation

```sh
curl -fsSL https://raw.githubusercontent.com/pawanpaudel93/agentpc/main/install.sh | sh
```

The installer:

1. downloads the latest release and verifies its checksum,
2. installs `agentpc` to `~/.local/bin` (no `sudo`) and adds it to your `PATH`,
3. installs QEMU with Homebrew if it's missing (installing Homebrew first if needed, which
   asks for your password once),
4. registers the MCP server with the agents it finds (`agentpc mcp-install`),
5. checks everything with `agentpc doctor`.

Run it again to upgrade. Installer options:

| Variable | Effect |
| --- | --- |
| `AGENTPC_VERSION=0.1.0` | Install a specific version |
| `AGENTPC_INSTALL_DIR=<dir>` | Install somewhere other than `~/.local/bin` |
| `AGENTPC_NO_MCP=1` | Skip registering the MCP server |

To build from source instead, see [Development](#development).

## Quick start

**Ubuntu:**

```sh
agentpc create ubuntu        # first run downloads the image (~1.2 GB); then ~1 s per VM
```

**Windows:** Microsoft's license doesn't allow redistributing Windows images, so you build
yours once from a Windows 11 ARM64 ISO:

```sh
brew install colima docker                                  # needed for the first build only
agentpc image build windows --iso ~/Downloads/<file>.iso   # once, ~12 min
agentpc create windows                                      # ~4 s per VM
```

Then ask your agent something like:

- *"Create an Ubuntu VM, open a terminal and run `uname -a`."*
- *"Reset ubuntu-1 and check that my install script works on a clean machine."*
- *"Open Notepad on windows-1, type a short note and show me a screenshot."*

To watch a VM yourself, open the viewer URL printed by `agentpc info <name>`.

## Using it with AI agents

agentpc is an MCP server (`agentpc mcp`, stdio). One server handles every VM.

### Claude plugin

In Claude Code, install the agentpc plugin. It bundles the MCP server with a skill that
teaches Claude when and how to use the VMs:

```text
/plugin marketplace add pawanpaudel93/agentpc
/plugin install agentpc@agentpc
```

The plugin runs the installed `agentpc` binary, so install that first
(see [Installation](#installation)).

### Other agents

**Register it** with every supported agent that's installed (the installer does this):

```sh
agentpc mcp-install                  # or pick: agentpc mcp-install claude claude-desktop codex
```

Supported: Claude Code, Claude Desktop, Codex, Cursor, Gemini CLI and VS Code (restart Claude
Desktop after registering). For any other MCP client, add:

```json
{
  "mcpServers": {
    "agentpc": { "command": "agentpc", "args": ["mcp"] }
  }
}
```

This repository also contains project-level configs (`.mcp.json`, `.codex/config.toml`,
`.cursor/mcp.json`, `.gemini/settings.json`, `.vscode/mcp.json`), so agents opened in a clone
pick the server up automatically.

### Tools

| Tool | Description |
| --- | --- |
| `vm_list` | VMs, their state, and the available images with their OS versions |
| `vm_create` | Create a VM from an image and wait until its desktop is ready |
| `vm_start` / `vm_stop` | Boot a stopped VM / shut one down cleanly |
| `vm_reset` | Discard all changes: back to a fresh copy of the image |
| `vm_delete` | Delete a VM and its disk |
| `vm_screenshot` | PNG screenshot from the hypervisor |
| `vm_exec` | Run a command: PowerShell on Windows, bash on Ubuntu |
| `desktop_tools` | List the desktop-control tools inside a VM |
| `desktop` | Call one of them: click, type, launch apps, read the UI tree, … |

| Guest | Desktop | Desktop-control server |
| --- | --- | --- |
| Windows | Windows 11 Pro ARM | [Windows-MCP](https://github.com/CursorTouch/Windows-MCP) |
| Ubuntu | Ubuntu 24.04, XFCE on X11 | [cua-driver](https://github.com/trycua/cua) (over SSH) |

[AGENTS.md](AGENTS.md) has usage tips for agents.

## CLI reference

### VMs

| Command | Description |
| --- | --- |
| `agentpc create <os> [name]` | Create a VM (`ubuntu` or `windows`); fetches the Ubuntu image if missing |
| `agentpc list` | VMs and images |
| `agentpc info <name>` | Viewer URL, SSH and VNC details |
| `agentpc start <name>` | Boot a stopped VM |
| `agentpc stop <name>` | Shut a VM down cleanly (its disk is kept) |
| `agentpc reset <name>` | Discard all changes: back to a fresh copy of the image |
| `agentpc rm <name>` | Delete a VM and its disk |
| `agentpc ssh <name> [command]` | Run a command, or open a shell with no command |
| `agentpc screenshot <name> [file]` | Save a PNG screenshot |

### Image commands

| Command | Description |
| --- | --- |
| `agentpc image pull ubuntu [--tag 24.04]` | Download the published Ubuntu image |
| `agentpc image build <os> [--iso <path>]` | Build an image locally (Ubuntu ~3 min, Windows ~12 min) |
| `agentpc image ls` | List local images with their OS versions |
| `agentpc image info <os>` | Version, source, build date and desktop server of an image |
| `agentpc image rm <os>` | Delete a local image |
| `agentpc image snapshot <os>` | Recapture the snapshot VMs resume from (build and pull do this) |
| `agentpc image push ubuntu` | Maintainers: publish the image to ghcr.io |

### Setup

| Command | Description |
| --- | --- |
| `agentpc mcp` | Run the MCP server on stdio (what agents launch) |
| `agentpc mcp-install [clients…]` | Register the MCP server with agents |
| `agentpc doctor` | Check prerequisites |

## Images

An **image** is a read-only disk with the OS, desktop and agent tools installed. Every VM is a
copy-on-write clone of an image, so a VM starts from a clean install and costs only a few MB.

| OS | How to get it | Source |
| --- | --- | --- |
| Ubuntu | `agentpc image pull ubuntu` (automatic on first `create`) or `agentpc image build ubuntu` | Official Ubuntu 24.04 cloud image |
| Windows | `agentpc image build windows --iso <path>` | Your Windows 11 ARM64 ISO |

Each image records what it is (`agentpc image info <os>`):

```json
{
  "os": "windows",
  "version": "Windows 11 Pro 24H2 (build 26100.4349)",
  "version_id": "11-24H2",
  "arch": "arm64",
  "base": "Windows 11 24H2 ISO, ARM64, consumer editions, build 26100.4349, en-us",
  "built": "20260926",
  "agentpc": "0.1.0",
  "desktop_server": "Windows-MCP 0.8.5",
  "iso_sha256": "f788b83e…"
}
```

Published Ubuntu images live at `ghcr.io/pawanpaudel93/agentpc-ubuntu` with these tags:

| Tag | Meaning |
| --- | --- |
| `latest` | Newest image |
| `24.04` | Newest build of Ubuntu 24.04 |
| `24.04-YYYYMMDD` | One specific build (pinned) |

## Configuration

| Variable | Default | Description |
| --- | --- | --- |
| `AGENTPC_HOME` | `~/.agentpc` | Where images, VMs, keys and caches live |
| `AGENTPC_IMAGE_REPO` | `ghcr.io/pawanpaudel93/agentpc` | Registry prefix for `image pull`/`push` (`<repo>-<os>:<tag>`) |
| `WIN_ISO` | `~/Downloads/*A64FRE*.iso` | Windows ISO used by `image build windows` without `--iso` |

Each VM gets its own ports on `127.0.0.1`, derived from its slot number `n`:

| Port | Use |
| --- | --- |
| `2200 + n` | SSH |
| `8000 + n` | Windows-MCP (Windows VMs) |
| `5910 + n` | VNC |
| `5700 + n` | VNC over WebSocket (for the viewer) |
| `8100` | Browser viewer, shared by all VMs |

The guest login is `agent` / `agent`.

## How it works

- **Hypervisor.** VMs run in QEMU with Apple's Hypervisor.framework (HVF), natively on Apple
  Silicon. No Docker or Linux VM sits in between.
- **Instant start.** After building or downloading an image, agentpc boots it once, waits
  until the desktop and its control server are running, and saves the VM's memory. New VMs
  resume from that saved state instead of booting (~1 s / ~4 s instead of ~14 s / ~25 s). A
  `start` after `stop` is a normal boot; `reset` resumes a fresh copy again.
- **Snapshots stay local.** A memory snapshot depends on the Mac's chip and QEMU version, so
  only the disk is published; the snapshot is recaptured after each pull (~35 s).
- **Image distribution.** Ubuntu images are OCI artifacts on GitHub Container Registry: a
  compressed qcow2 split into 512 MB parts, downloaded in parallel and checksum-verified.
- **Windows build.** [dockur/windows-arm](https://github.com/dockur/windows-arm) prepares a setup
  disk once (unattended-install answer file and ARM virtio drivers); a first-logon script
  installs OpenSSH and Windows-MCP.
- **Ubuntu build.** The official cloud image is provisioned with cloud-init: XFCE on X11,
  auto-login, and cua-driver. cloud-init is then disabled so clones don't re-provision.
- **Why not Docker?** dockur can't run Windows on a Mac: Apple's virtualization gives nested
  VMs no performance-monitoring unit, and Windows ARM hangs at boot without one.

## Troubleshooting

- **Check the setup:** `agentpc doctor`.
- **See the screen:** `agentpc screenshot <name>`, or open the viewer URL from
  `agentpc info <name>`.
- **Logs** for each VM are in `~/.agentpc/instances/<name>/`: `qemu.log` (QEMU errors) and
  `serial.log` (guest console).
- **A VM is in a bad state:** `agentpc reset <name>`.
- **`image build`/`pull`/`rm` refuses:** VMs still depend on that image; `agentpc rm` them
  first.

## Security

- Everything listens on `127.0.0.1` only.
- The desktop-control servers inside the VMs are unauthenticated; any process on your Mac can
  reach them.
- Guests use the fixed login `agent` / `agent`.
- Treat VMs as disposable sandboxes, not as a place for secrets.

## Development

```sh
cargo build --release                  # target/release/agentpc
cargo clippy --all-targets -- -D warnings
cargo test
```

Guest provisioning files in `guests/` are embedded into the binary. [AGENTS.md](AGENTS.md)
describes the code layout for contributors and coding agents.

<details>
<summary>Releasing (maintainers)</summary>

1. Bump `version` in `Cargo.toml`, commit, then tag and push `vX.Y.Z`. The release workflow
   builds the binary tarball and the MCP bundle (`agentpc-X.Y.Z.mcpb`), with `.sha256` files
   and a filled-in `server.json`, and attaches them to the GitHub Release.
2. Publish the Ubuntu image: `agentpc image build ubuntu`, `oras login ghcr.io`, then
   `agentpc image push ubuntu`. Make the ghcr.io package public once in its package settings.
3. Publish to the MCP Registry: download the release's `server.json` over the repo copy, then
   `brew install mcp-publisher`, `mcp-publisher login github`, `mcp-publisher publish`.

</details>

## License

[MIT](LICENSE) © 2026 Pawan Paudel
