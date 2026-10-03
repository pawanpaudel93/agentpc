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
| `run_command(name, command, timeout?, background?)` | Shell over SSH: PowerShell on windows, bash on ubuntu and arch. Returns exit code, stdout, stderr (long output trimmed). Foreground runs are killed at `timeout` (default 120 s) with partial output; `background: true` returns a job id you poll with `get_job_status`. |
| `get_job_status(name, id, tail_lines?)` | State of a background job (running, or exited with its code) plus its log tail. |
| `upload_file` / `download_file` | Copy files or folders between the Mac and a VM. |
| `forward_port(name, guest_port, host_port?, protocol?)` | Reach a server in the VM from the Mac at `127.0.0.1:<host_port>` (a free port if omitted). TCP (default) is an SSH tunnel: reaches a server on the guest's own `127.0.0.1`. `protocol: "udp"` is a QEMU host forward: the guest server must listen on `0.0.0.0`; not on offline VMs. Lasts until the VM stops. |
| `list_forwards(name)` / `delete_forward(name, host_port, protocol?)` | List a VM's forwards (with protocol) / stop one. |
| `read_vm_log(name, which, tail_lines?)` | Tail a VM's `qemu` or `serial` log when it won't boot or the desktop is unreachable. |
| `list_desktop_tools(name, tool?)` | Desktop-control tools in that VM with their required arguments and read-only marks, or one tool's full schema. |
| `use_desktop_tool(name, tool, arguments?)` | Call one of those tools (click, type, launch, snapshot…). Wrong arguments or a wrong name return the argument list or close matches. |

Work in a loop: look (`take_screenshot` or a snapshot tool) → act (`use_desktop_tool`) → look again to
verify. Prefer `run_command` for anything a shell can do; use `use_desktop_tool` for GUI-only work.

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

Guest tips:

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
  VM is ready again) or retry `run_command`.
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

Rules:

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
  helper tests, plugin validation; `release.sh` runs it too) plus a real instance (`agentpc create
  ubuntu`, then the MCP tools). Unit tests can't cover the VM paths. After changing `guests/`, the desktop
  driver or the MCP gateway, rebuild the affected images and run `scripts/smoke.py --bin
  target/release/agentpc`: it drives fresh VMs through the MCP server like an agent does.
- Never commit `target/`.
