//! agentpc: instant, resettable Windows, Ubuntu and Arch Linux ARM desktop VMs for AI agents on Apple Silicon Macs.

mod execution;
mod guest;
mod image;
mod instance;
mod jobs;
mod mcp;
mod ops;
mod qemu;
mod readiness;
mod registry;
mod setup;
mod update;
mod viewer;

use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};

use instance::{Image, Instance};

/// Timestamped progress line on stderr (stdout stays clean for results and MCP).
#[macro_export]
macro_rules! log {
    ($($arg:tt)*) => {{
        let line = format!($($arg)*);
        eprintln!("[{}] {}", $crate::instance::local_hms(), line);
        $crate::ops::progress(&line);
    }};
}

#[derive(Parser)]
#[command(
    name = "agentpc",
    version,
    about = "Instant, resettable Windows, Ubuntu and Arch Linux ARM desktops for AI agents"
)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Create a VM from an image and boot it (default name: <os>-<n>); gets the image if
    /// missing. With --from, from a checkpoint of another VM instead (it boots cold)
    Create {
        /// ubuntu, windows, arch, or a version: ubuntu-22.04, ubuntu-x86apps, arch-x86apps, windows-11-23h2 (see image build --help). Optional with --from (then it must be the source's image)
        #[arg(required_unless_present = "from")]
        image: Option<String>,
        /// VM name: up to 64 letters, digits, . - _ (default: <os>-<n>)
        name: Option<String>,
        /// VM name, as a flag (instead of the second argument)
        #[arg(long = "name", value_name = "NAME", conflicts_with = "name")]
        name_flag: Option<String>,
        /// Make the VM from checkpoint <vm>/<label>: a copy of its disk, booted cold; its memory, port forwards and job records are not carried over
        #[arg(long, value_name = "VM/LABEL")]
        from: Option<String>,
        /// Memory in GB (default 8 Windows, 4 Ubuntu and Arch); a non-default size cold-boots
        #[arg(long)]
        memory: Option<u32>,
        /// CPUs (default 4); a non-default count cold-boots
        #[arg(long)]
        cpus: Option<u32>,
        /// No internet or access to this Mac (SSH, the viewer and forwarded ports still work)
        #[arg(long)]
        offline: bool,
        /// Open the ready VM's desktop viewer in this Mac's default browser
        #[arg(long, visible_alias = "open-in-browser")]
        open: bool,
    },
    /// List VMs and images
    #[command(visible_alias = "ls")]
    List {
        /// Print JSON (the same data as the MCP list_vms tool)
        #[arg(long)]
        json: bool,
    },
    /// Viewer URL, SSH and VNC details, checkpoints, and on x86apps VMs how x86 programs run
    /// ("x86 programs: FEX, hardware|emulated TSO")
    Info {
        /// VM name
        name: String,
    },
    /// Boot stopped VMs
    Start {
        /// VM names
        #[arg(required_unless_present = "all")]
        names: Vec<String>,
        /// Start every stopped VM
        #[arg(long, conflicts_with = "names")]
        all: bool,
    },
    /// Shut VMs down cleanly (their disks are kept)
    Stop {
        /// VM names
        #[arg(required_unless_present = "all")]
        names: Vec<String>,
        /// Stop every running VM
        #[arg(long, conflicts_with = "names")]
        all: bool,
    },
    /// Discard all changes: back to a fresh copy of the image
    Reset {
        /// VM names
        #[arg(required = true)]
        names: Vec<String>,
    },
    /// Delete VMs, their disks and checkpoints
    #[command(visible_alias = "delete")]
    Rm {
        /// VM names
        #[arg(required = true)]
        names: Vec<String>,
    },
    /// Save a VM's disk and memory under a label (a running VM pauses for a few seconds)
    Checkpoint {
        /// VM name
        name: String,
        /// Checkpoint name: letters, digits, . - _
        label: String,
        /// Delete the checkpoint instead
        #[arg(short, long)]
        delete: bool,
    },
    /// Put a VM back exactly as it was at a checkpoint (resumes in seconds)
    Restore {
        /// VM name
        name: String,
        /// Checkpoint name (agentpc info <name> lists them)
        label: String,
    },
    /// Shell into a VM, or run a command (PowerShell on Windows, bash on Ubuntu and Arch)
    Ssh {
        /// VM name
        name: String,
        /// Command to run; an interactive shell if omitted
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        command: Vec<String>,
    },
    /// Copy files between this Mac and a VM: `agentpc cp ./app.msi windows-1:Downloads/`
    Cp {
        /// Source: a path on this Mac, or <vm>:<path>
        src: String,
        /// Destination: a path on this Mac, or <vm>:<path>
        dst: String,
    },
    /// Forward 127.0.0.1:<host_port> to a port inside a running VM (free port if omitted).
    /// TCP reaches servers on the guest's own 127.0.0.1; --udp needs the server on 0.0.0.0
    Forward {
        /// VM name
        name: String,
        /// Port a server listens on inside the VM (omit with --list/--rm)
        guest_port: Option<u16>,
        /// Port on this Mac (default: a free one)
        host_port: Option<u16>,
        /// List this VM's active forwards instead of adding one
        #[arg(long)]
        list: bool,
        /// Stop forwarding this host port (both protocols unless --udp or --tcp is given)
        #[arg(long, value_name = "HOST_PORT")]
        rm: Option<u16>,
        /// Forward UDP (a QEMU host forward) instead of TCP; not on --offline VMs
        #[arg(long, conflicts_with = "tcp")]
        udp: bool,
        /// With --rm: only the TCP forward on that port
        #[arg(long, requires = "rm")]
        tcp: bool,
    },
    /// Save a PNG screenshot
    Screenshot {
        /// VM name
        name: String,
        /// Output file (default: <vm dir>/screen.png)
        out: Option<PathBuf>,
    },
    /// Manage images (the installed OS every VM is cloned from)
    #[command(subcommand)]
    Image(ImageCmd),
    /// Run the MCP server on stdio (what agents launch)
    Mcp,
    /// Register the MCP server with installed agents
    McpInstall {
        /// claude, claude-desktop, codex, cursor, gemini, vscode (default: every one installed)
        clients: Vec<String>,
    },
    /// Remove the MCP server from agents
    McpUninstall {
        /// claude, claude-desktop, codex, cursor, gemini, vscode (default: all)
        clients: Vec<String>,
    },
    /// Free disk space: downloaded ISOs and cloud images, and leftovers of interrupted work
    ///
    /// Removes downloads fetched again when needed (Windows ISOs, cloud images, virtio drivers,
    /// the Arch Linux ARM tarball), leftovers of interrupted builds, pulls, pushes and
    /// checkpoints, and other agentpc versions' TSO libraries. Never touches images or VMs, and
    /// keeps what a running build, pull or push is using; lists images no VM uses.
    Clean {
        /// Only show what would be deleted
        #[arg(short = 'n', long)]
        dry_run: bool,
    },
    /// Remove agentpc: stops VMs, unregisters agents, deletes ~/.agentpc and this binary
    Uninstall {
        /// Keep images, VMs and keys in ~/.agentpc
        #[arg(long)]
        keep_data: bool,
        /// Don't ask for confirmation
        #[arg(short, long)]
        yes: bool,
    },
    /// Update agentpc to the latest release (checksum-verified; images and VMs are kept)
    #[command(visible_alias = "upgrade")]
    Update {
        /// Only say whether a newer release exists
        #[arg(long)]
        check: bool,
    },
    /// Check prerequisites: Apple Silicon, macOS and QEMU versions, free disk, images
    Doctor,
    /// Print a shell completion script: agentpc completions zsh > ~/.zfunc/_agentpc
    Completions {
        /// bash, zsh, fish, elvish or powershell
        shell: clap_complete::Shell,
    },
    #[command(name = "__viewer", hide = true)]
    Viewer,
}

#[derive(Subcommand)]
enum ImageCmd {
    /// Build an image locally by installing the OS (Ubuntu ~3 min, Arch ~6 min, ubuntu-x86apps ~8 min,
    /// arch-x86apps ~10 min, Windows ~12 min)
    #[command(after_help = "\
Images are <os>-<version>; a bare os means the default version.
  ubuntu-<release>            any release at cloud-images.ubuntu.com/releases (default 24.04)
  ubuntu-<release>-x86apps    also runs x86_64/i386 Linux programs, through FEX (22.04, 24.04)
  windows-11-25h2             Windows 11 25H2 Home/Pro (default: windows, windows-11)
  windows-11-24h2             Windows 11 24H2 Home/Pro (archive mirror)
  windows-11-23h2             Windows 11 23H2 Home/Pro (archive mirror)
  arch-rolling                Arch Linux ARM (default: arch), installed from an Ubuntu helper VM
  arch-rolling-x86apps        Arch that also runs x86_64/i386 Linux programs, through FEX (arch-x86apps)
Dated builds (ubuntu-24.04-YYYYMMDD, arch-rolling-x86apps-YYYYMMDD, ...) can only be pulled.
Build and rm refuse while a running build uses the image (an Arch build runs in a clone of
the Ubuntu image); rm also while a pull or snapshot of it runs.
ISOs are checksum-verified. --iso installs your own: it must match a release name above,
or use any other name (windows-custom).")]
    Build {
        /// Image to build, e.g. ubuntu, ubuntu-22.04, ubuntu-x86apps, arch, arch-x86apps, windows, windows-11-24h2
        image: String,
        /// Windows ARM64 ISO to install from (default: $WIN_ISO, an earlier download,
        /// a Home/Pro ISO of that release in ~/Downloads, else download it)
        #[arg(long)]
        iso: Option<PathBuf>,
    },
    /// Download a published image, e.g. ubuntu-22.04, ubuntu-x86apps, arch or arch-x86apps
    /// (Windows can't be redistributed); pulling again after a failure resumes it
    Pull {
        /// Image, e.g. ubuntu, ubuntu-22.04, ubuntu-x86apps, arch, arch-x86apps, or a pinned
        /// build such as ubuntu-24.04-YYYYMMDD or arch-rolling-YYYYMMDD
        image: String,
    },
    /// Publish a local image to the registry (maintainers; needs `oras login`)
    Push {
        /// Image, e.g. ubuntu-24.04
        image: String,
    },
    /// List local images
    #[command(visible_alias = "list")]
    Ls,
    /// Everything recorded about an image: OS version, edition, build, source, tool versions
    Info {
        /// Image, e.g. ubuntu-24.04 or windows
        image: String,
    },
    /// Delete local images (refused while VMs, or a build, pull or snapshot, use one)
    #[command(visible_alias = "delete")]
    Rm {
        /// Images, e.g. windows-11-24h2
        #[arg(required = true)]
        images: Vec<String>,
    },
    /// Apply this agentpc's guest setup to the image and recapture the snapshot VMs resume from (build and pull do this; its VMs must be deleted first)
    Snapshot {
        /// Image, e.g. ubuntu-24.04
        image: String,
    },
}

fn main() {
    ensure_path();
    instance::secure_home();
    if let Err(e) = instance::migrate_legacy_images() {
        log!("upgrading the image layout failed: {e:#}");
    }
    if let Err(e) = run(Cli::parse()) {
        log!("FAIL: {e:#}");
        std::process::exit(1);
    }
}

/// `create --from <vm>/<label> copy`: a lone word that names no image is the new VM's name, so
/// it isn't refused as an image that doesn't match the checkpoint's.
fn checkpoint_args(
    image: Option<String>,
    name: Option<String>,
) -> (Option<String>, Option<String>) {
    match (image, name) {
        (Some(word), None) if Image::resolve(&word).is_err() => (None, Some(word)),
        args => args,
    }
}

fn run(cli: Cli) -> Result<()> {
    match cli.cmd {
        Cmd::Create {
            image,
            name,
            name_flag,
            from,
            memory,
            cpus,
            offline,
            open,
        } => {
            let name = name.or(name_flag);
            let made = match from {
                Some(from) => {
                    let (image, name) = checkpoint_args(image, name);
                    ops::create_from_checkpoint(
                        &ops::CheckpointRef::parse(&from)?,
                        image.as_deref(),
                        name.as_deref(),
                        memory,
                        cpus,
                        offline,
                        None,
                    )
                }
                None => {
                    let image = image.context(
                        "an image is required (ubuntu, windows, arch, ...) unless --from is given",
                    )?;
                    ops::create(
                        &Image::resolve(&image)?,
                        name.as_deref(),
                        memory,
                        cpus,
                        offline,
                        None,
                    )
                }
            };
            out(viewer::open_created(made, open))
        }
        Cmd::List { json } => out(if json {
            ops::list_json()
        } else {
            ops::list_table()
        }),
        Cmd::Info { name } => out(Ok(ops::info(&Instance::load(&name)?))),
        Cmd::Start { names, all } => for_each_vm(&names, all.then_some(false), ops::boot),
        Cmd::Stop { names, all } => for_each_vm(&names, all.then_some(true), ops::stop),
        Cmd::Reset { names } => for_each_vm(&names, None, ops::reset),
        Cmd::Rm { names } => for_each_vm(&names, None, ops::delete),
        Cmd::Checkpoint {
            name,
            label,
            delete,
        } => {
            let inst = Instance::load(&name)?;
            out(if delete {
                ops::delete_checkpoint(&inst, &label)
            } else {
                ops::checkpoint(&inst, &label)
            })
        }
        Cmd::Restore { name, label } => out(ops::restore(&Instance::load(&name)?, &label)),
        Cmd::Ssh { name, command } => {
            let inst = Instance::load(&name)?;
            let remote = command.join(" ");
            let mut args = ops::ssh_args(&inst, &remote);
            if remote.is_empty() {
                args.pop();
                args.insert(0, "-t".into());
            }
            use std::os::unix::process::CommandExt;
            Err(std::process::Command::new("ssh").args(args).exec().into())
        }
        Cmd::Cp { src, dst } => out(copy(&src, &dst)),
        Cmd::Forward {
            name,
            guest_port,
            host_port,
            list,
            rm,
            udp,
            tcp,
        } => {
            let inst = Instance::load(&name)?;
            let protocol = if udp {
                ops::Protocol::Udp
            } else {
                ops::Protocol::Tcp
            };
            if list {
                out(Ok(ops::forwards_text(&inst)))
            } else if let Some(port) = rm {
                out(ops::remove_forward(
                    &inst,
                    port,
                    (udp || tcp).then_some(protocol),
                ))
            } else {
                let guest_port =
                    guest_port.context("guest_port is required (or use --list / --rm)")?;
                out(ops::forward(&inst, guest_port, host_port, protocol))
            }
        }
        Cmd::Screenshot { name, out: path } => {
            let inst = Instance::load(&name)?;
            let path = path.unwrap_or_else(|| inst.dir.join("screen.png"));
            let path = std::path::absolute(path)?;
            qemu::screenshot(&inst, &path)?;
            println!("{}", path.display());
            Ok(())
        }
        Cmd::Image(cmd) => match cmd {
            ImageCmd::Build { image: i, iso } => image::build(&i.parse()?, iso),
            ImageCmd::Pull { image: i } => registry::pull(&i.parse()?),
            ImageCmd::Push { image: i } => registry::push(&i.parse()?),
            ImageCmd::Ls => out(image::list()),
            ImageCmd::Info { image: i } => out(image::describe(&Image::resolve(&i)?)),
            ImageCmd::Rm { images } => {
                let images: Vec<Image> = images.iter().map(|i| i.parse()).collect::<Result<_>>()?;
                each(images, |i| image::remove(&i))
            }
            ImageCmd::Snapshot { image: i } => image::snapshot(&i.parse::<Image>()?),
        },
        Cmd::Mcp => tokio::runtime::Runtime::new()?.block_on(mcp::serve()),
        Cmd::McpInstall { clients } => setup::mcp_install(&clients),
        Cmd::McpUninstall { clients } => setup::mcp_uninstall(&clients),
        Cmd::Clean { dry_run } => out(setup::clean(dry_run)),
        Cmd::Uninstall { keep_data, yes } => setup::uninstall(keep_data, yes),
        Cmd::Update { check } => update::update(check),
        Cmd::Doctor => {
            if !setup::doctor()? {
                std::process::exit(1);
            }
            Ok(())
        }
        Cmd::Completions { shell } => {
            use clap::CommandFactory;
            clap_complete::generate(
                shell,
                &mut Cli::command(),
                "agentpc",
                &mut std::io::stdout(),
            );
            Ok(())
        }
        Cmd::Viewer => viewer::serve(),
    }
}

/// Apps launched from the Dock (Claude Desktop, Cursor, VS Code) start MCP servers with a
/// minimal PATH that lacks Homebrew, where QEMU lives.
fn ensure_path() {
    let path = std::env::var("PATH").unwrap_or_default();
    let mut dirs: Vec<&str> = path.split(':').filter(|d| !d.is_empty()).collect();
    for d in [
        "/opt/homebrew/bin",
        "/usr/local/bin",
        "/usr/bin",
        "/bin",
        "/usr/sbin",
        "/sbin",
    ] {
        if !dirs.contains(&d) {
            dirs.push(d);
        }
    }
    // SAFETY: runs first thing in main, before any other thread exists.
    unsafe { std::env::set_var("PATH", dirs.join(":")) };
}

/// `agentpc cp` endpoints: `<vm>:<path>` inside a VM, anything else on this Mac.
fn copy(src: &str, dst: &str) -> Result<String> {
    let guest = |arg: &str| {
        arg.split_once(':')
            .and_then(|(vm, path)| Instance::load(vm).ok().map(|i| (i, path.to_string())))
    };
    match (guest(src), guest(dst)) {
        (None, Some((inst, path))) => ops::upload(&inst, &std::path::absolute(src)?, &path),
        (Some((inst, path)), None) => ops::download(&inst, &path, &std::path::absolute(dst)?),
        _ => anyhow::bail!(
            "exactly one side must be <vm>:<path>, e.g. {} cp ./file ubuntu-1:/tmp/",
            crate::setup::cmd_name()
        ),
    }
}

/// Run `op` on the named VMs, or with `all = Some(running)` on every VM in that state.
/// Names are checked before anything runs; one failure doesn't stop the rest.
fn for_each_vm(
    names: &[String],
    all: Option<bool>,
    op: fn(&Instance) -> Result<String>,
) -> Result<()> {
    let targets: Vec<Instance> = match all {
        Some(running) => Instance::list()?
            .into_iter()
            .filter(|i| i.running() == running)
            .collect(),
        None => names
            .iter()
            .map(|n| Instance::load(n))
            .collect::<Result<_>>()?,
    };
    if targets.is_empty() {
        println!("no VMs to act on");
        return Ok(());
    }
    each(targets, |i| op(&i))
}

fn each<T>(items: Vec<T>, op: impl Fn(T) -> Result<String>) -> Result<()> {
    let mut failed = 0;
    for item in items {
        match op(item) {
            Ok(s) => println!("{}", s.trim_end()),
            Err(e) => {
                log!("FAIL: {e:#}");
                failed += 1;
            }
        }
    }
    if failed > 0 {
        anyhow::bail!("{failed} of them failed");
    }
    Ok(())
}

fn out(r: Result<String>) -> Result<()> {
    println!("{}", r?.trim_end());
    Ok(())
}

#[cfg(test)]
mod cli_tests {
    use super::*;

    #[test]
    fn browser_open_is_opt_in_with_a_visible_alias() {
        for (flags, expected) in [
            (vec![], false),
            (vec!["--open"], true),
            (vec!["--open-in-browser"], true),
        ] {
            let mut args = vec!["agentpc", "create", "ubuntu"];
            args.extend(flags);
            let cli = Cli::try_parse_from(args).unwrap();
            let Cmd::Create { open, .. } = cli.cmd else {
                panic!("not create")
            };
            assert_eq!(open, expected);
        }
    }

    fn create(args: &[&str]) -> Result<Cmd, clap::Error> {
        let mut argv = vec!["agentpc", "create"];
        argv.extend(args);
        Cli::try_parse_from(argv).map(|c| c.cmd)
    }

    #[test]
    fn create_takes_a_checkpoint_instead_of_an_image() {
        let Cmd::Create {
            image,
            name,
            name_flag,
            from,
            ..
        } = create(&["--from", "src/clean", "--name", "copy"]).unwrap()
        else {
            panic!("not create")
        };
        assert_eq!(image, None);
        assert_eq!(name, None);
        assert_eq!(name_flag.as_deref(), Some("copy"));
        assert_eq!(from.as_deref(), Some("src/clean"));
        // An image may still be named; it must then be the source's (checked by ops).
        let Cmd::Create { image, from, .. } = create(&["ubuntu", "--from", "src/clean"]).unwrap()
        else {
            panic!("not create")
        };
        assert_eq!(
            (image.as_deref(), from.as_deref()),
            (Some("ubuntu"), Some("src/clean"))
        );
    }

    #[test]
    fn create_needs_an_image_or_a_checkpoint_and_one_name() {
        assert!(create(&[]).is_err(), "no image and no --from");
        assert!(
            create(&["ubuntu", "vm1", "--name", "vm2"]).is_err(),
            "two names"
        );
        let Cmd::Create { name_flag, .. } = create(&["ubuntu", "--name", "vm1"]).unwrap() else {
            panic!("not create")
        };
        assert_eq!(name_flag.as_deref(), Some("vm1"));
    }

    #[test]
    fn with_from_a_lone_word_that_is_no_image_is_the_name() {
        use super::checkpoint_args;
        let s = |v: &str| Some(v.to_string());
        assert_eq!(checkpoint_args(s("copy"), None), (None, s("copy")));
        // An image stays an image (checked against the source), as does a word next to a name.
        assert_eq!(checkpoint_args(s("ubuntu"), None), (s("ubuntu"), None));
        assert_eq!(
            checkpoint_args(s("ubuntu-22.04"), s("copy")),
            (s("ubuntu-22.04"), s("copy"))
        );
        assert_eq!(checkpoint_args(None, s("copy")), (None, s("copy")));
    }
}
