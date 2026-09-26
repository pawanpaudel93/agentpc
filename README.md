# agentpc

Disposable Windows 11 and Ubuntu desktop VMs on an Apple Silicon Mac, controllable by AI
coding agents (Claude Code, Codex, Gemini CLI, Cursor, VS Code, or any MCP client).
One Rust binary, `agentpc`: a CLI and, via `agentpc mcp`, an MCP stdio server.
Native QEMU + HVF; new VMs are ready in about a second.

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
agentpc create ubuntu      # first time: downloads the Ubuntu image (~1.2 GB); then ~1 s per VM
```

Then ask your agent things like *"open a terminal on ubuntu-1 and run uname -a"* or
*"reset ubuntu-1 and check that my install script works on a clean machine"*.

Windows images can't be redistributed (Microsoft's license), so you build yours once from a
Windows 11 ARM64 ISO. The first build also needs colima + docker
(`brew install colima docker`) to prepare the setup disk:

```sh
agentpc image build windows --iso ~/Downloads/<file>.iso   # once, ~12 min
agentpc create windows                                      # ~4 s per VM
```

Without `--iso`, the build uses `$WIN_ISO` or `~/Downloads/*A64FRE*.iso`.

## MCP server

One server, `agentpc`, for all VMs. `agentpc mcp-install` registers it with Claude
Code, Codex, Cursor, Gemini CLI and VS Code (user scope). For any other client:

```json
{ "mcpServers": { "agentpc": { "command": "agentpc", "args": ["mcp"] } } }
```

This repo also ships project configs (`.mcp.json`, `.codex/config.toml`,
`.gemini/settings.json`, `.cursor/mcp.json`, `.vscode/mcp.json`) that launch the installed
binary.

| Tool | Use |
|------|-----|
| `vm_list` | VMs, their state, and which images exist |
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
agentpc create ubuntu|windows [name]       # new VM (default name <os>-<n>); gets the image if missing
agentpc list                               # VMs and images
agentpc info ubuntu-1                      # viewer URL, SSH, VNC
agentpc start|stop|reset|rm <name>         # reset = back to a fresh copy of the image
agentpc ssh windows-1 'Get-Process'        # no command = interactive shell
agentpc screenshot ubuntu-1 [out.png]      # PNG screenshot

agentpc image pull ubuntu [--tag 24.04]    # download the published Ubuntu image
agentpc image build ubuntu|windows [--iso PATH]  # build an image locally instead
agentpc image ls | rm <os>                 # list (with OS version) / delete local images
agentpc image info windows                 # version, source (ISO + sha256), build date, desktop server
agentpc image snapshot <os>                # recapture the RAM snapshot (build/pull do this)
agentpc image push ubuntu                  # maintainers: publish to ghcr.io (needs oras login)
agentpc mcp                                # MCP server on stdio (what agents launch)
agentpc mcp-install [clients...]           # register with claude, codex, cursor, gemini, vscode
agentpc doctor                             # check prerequisites
```

State lives in `~/.agentpc` (override with `AGENTPC_HOME`). Every VM has a browser viewer
(noVNC) on the shared viewer at `http://127.0.0.1:8100`; `agentpc info` prints its URL.

## How it works

- **Images + copy-on-write VMs.** An *image* is a read-only disk with the OS, desktop and
  agent tools installed (`image build`, or `image pull` for Ubuntu). agentpc then boots it once
  and saves its RAM with the desktop already running (its *snapshot*). `create` makes a qcow2
  overlay on the image and resumes that RAM, so a VM costs a few MB and is ready in ~1 s
  (Ubuntu) or ~4 s (Windows) instead of booting (~14 s / ~25 s). `start` after `stop` is a
  cold boot; `reset` resumes a fresh copy again. Replacing an image would break the VMs built
  on it, so `image build/pull/rm` refuse while any exist.
- **Published images** live on GitHub Container Registry as OCI artifacts
  (`ghcr.io/pawanpaudel93/agentpc-ubuntu`): a compressed qcow2 in 512 MB parts, downloaded in
  parallel and checksum-verified. Only the disk is published; the RAM snapshot depends on the
  Mac's chip and QEMU version, so it is recaptured locally after each pull (~35 s). The Ubuntu
  image is based on the official Ubuntu 24.04 cloud image.
- **Versions.** Each image records what it is in `~/.agentpc/images/<os>.json`: the OS version
  as the guest reports it (e.g. `Ubuntu 24.04.5 LTS`, `Windows 11 Pro 24H2 (build 26100.4349)`),
  what it was built from (the cloud-image serial or the Windows ISO), and the build date. The
  same record is the published image's OCI config, and pushes are tagged `:latest`, `:24.04`
  (newest build of that release) and `:24.04-YYYYMMDD` (pinned). `image ls`, `list` and the
  MCP `vm_list` tool show the version.
- **Screenshots** come straight from QEMU as PNG in ~40 ms, independent of the guest.
- **Windows:** dockur/windows-arm builds `setup.img` once (answer file + ARM virtio
  drivers); the OEM script installs Windows-MCP at first logon.
- **Ubuntu:** the cloud image + cloud-init installs XFCE on X11 and cua-driver; cloud-init
  is disabled after the build so VMs don't re-provision.
- **Why not Docker?** dockur can't boot Windows on a Mac: Apple's vz gives nested KVM no
  PMU, and Windows ARM hangs. agentpc runs QEMU natively with HVF instead.

## Security

Everything binds to 127.0.0.1. The desktop-control servers inside the guests are
unauthenticated, but reachable only from this Mac. The guest login is `agent` / `agent`.
Treat VMs as disposable sandboxes, not as security boundaries for secrets.

## Build from source

```sh
cargo build --release          # target/release/agentpc
```

Guest assets under `guests/` are embedded in the binary. MIT licensed.

## Releasing (maintainers)

1. Bump `version` in `Cargo.toml`, commit, then tag and push `vX.Y.Z`. The Release workflow
   builds the tarball, the MCP bundle (`agentpc-X.Y.Z.mcpb`), their `.sha256` files, and a
   filled-in `server.json`, and attaches them all to the GitHub Release.
2. Publish the Ubuntu image: `agentpc image build ubuntu`, then `oras login ghcr.io` and
   `agentpc image push ubuntu` (tags `:latest` and the date). Make the ghcr.io package public
   once in its GitHub package settings.
3. Publish to the MCP Registry: download that release's `server.json` over the repo copy,
   then `brew install mcp-publisher`, `mcp-publisher login github`, `mcp-publisher publish`.
