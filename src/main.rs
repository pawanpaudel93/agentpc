//! agentpc: disposable Windows 11 / Ubuntu desktop VMs for AI agents on Apple Silicon.

mod image;
mod instance;
mod mcp;
mod ops;
mod qemu;
mod registry;
mod setup;
mod viewer;

use std::path::PathBuf;

use anyhow::Result;
use clap::{Parser, Subcommand};

use instance::{Image, Instance};

/// Timestamped progress line on stderr (stdout stays clean for results and MCP).
#[macro_export]
macro_rules! log {
    ($($arg:tt)*) => {
        eprintln!("[{}] {}", $crate::instance::local_hms(), format!($($arg)*))
    };
}

#[derive(Parser)]
#[command(
    name = "agentpc",
    version,
    about = "Disposable Windows and Ubuntu desktops for AI agents"
)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Create a VM from an image and boot it (default name: <os>-<n>); gets the image if missing
    Create {
        /// ubuntu, windows, or a version: ubuntu-22.04, windows-11-23h2 (see image build --help)
        image: String,
        name: Option<String>,
        /// Memory in GB (default 8 Windows, 4 Ubuntu); a non-default size cold-boots
        #[arg(long)]
        memory: Option<u32>,
        /// CPUs (default 4); a non-default count cold-boots
        #[arg(long)]
        cpus: Option<u32>,
    },
    /// List VMs and images
    List,
    /// Viewer URL, SSH and VNC details
    Info { name: String },
    /// Boot a stopped VM
    Start { name: String },
    /// Shut a VM down cleanly (its disk is kept), or every running VM with --all
    Stop {
        name: Option<String>,
        /// Stop every running VM
        #[arg(long, conflicts_with = "name")]
        all: bool,
    },
    /// Discard all changes: back to a fresh copy of the image
    Reset { name: String },
    /// Delete a VM and its disk
    Rm { name: String },
    /// Save a VM's disk and memory under a label (a running VM pauses for a few seconds)
    Checkpoint {
        name: String,
        label: String,
        /// Delete the checkpoint instead
        #[arg(long)]
        delete: bool,
    },
    /// Put a VM back exactly as it was at a checkpoint (resumes in seconds)
    Restore { name: String, label: String },
    /// Shell into a VM, or run a command (PowerShell on Windows, bash on Ubuntu)
    Ssh {
        name: String,
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        command: Vec<String>,
    },
    /// Copy files between this Mac and a VM: `agentpc cp ./app.msi windows-1:Downloads/`
    Cp { src: String, dst: String },
    /// Forward 127.0.0.1:<host_port> to a port inside a running VM (free port if omitted)
    Forward {
        name: String,
        guest_port: u16,
        host_port: Option<u16>,
    },
    /// Save a PNG screenshot
    Screenshot { name: String, out: Option<PathBuf> },
    /// Manage images (the installed OS every VM is cloned from)
    #[command(subcommand)]
    Image(ImageCmd),
    /// Run the MCP server on stdio (what agents launch)
    Mcp,
    /// Register the MCP server with installed agents (claude, claude-desktop, codex, cursor, gemini, vscode)
    McpInstall { clients: Vec<String> },
    /// Remove the MCP server from agents (all when none given)
    McpUninstall { clients: Vec<String> },
    /// Free disk space: downloaded ISOs and cloud images, and leftovers of interrupted work
    Clean {
        /// Only show what would be deleted
        #[arg(long)]
        dry_run: bool,
    },
    /// Remove agentpc: stops VMs, unregisters agents, deletes ~/.agentpc and this binary
    Uninstall {
        /// Keep images, VMs and keys in ~/.agentpc
        #[arg(long)]
        keep_data: bool,
        /// Don't ask for confirmation
        #[arg(long)]
        yes: bool,
    },
    /// Check prerequisites
    Doctor,
    #[command(name = "__viewer", hide = true)]
    Viewer,
}

#[derive(Subcommand)]
enum ImageCmd {
    /// Build an image locally by installing the OS (Ubuntu ~3 min, Windows ~12 min)
    #[command(after_help = "\
Images are <os>-<version>; a bare os means the default version.
  ubuntu-<release>            any release at cloud-images.ubuntu.com/releases (default 24.04)
  windows-11-25h2             Windows 11 25H2 Home/Pro (default: windows, windows-11)
  windows-11-24h2             Windows 11 24H2 Home/Pro (archive mirror)
  windows-11-23h2             Windows 11 23H2 Home/Pro (archive mirror)
ISOs are checksum-verified. --iso installs your own: it must match a release name above,
or use any other name (windows-custom).")]
    Build {
        image: String,
        /// Windows ARM64 ISO to install from (default: $WIN_ISO, an earlier download,
        /// a Home/Pro ISO of that release in ~/Downloads, else download it)
        #[arg(long)]
        iso: Option<PathBuf>,
    },
    /// Download a published image, e.g. ubuntu-22.04 (Ubuntu only; Windows can't be redistributed)
    Pull { image: String },
    /// Publish a local image to the registry (maintainers; needs `oras login`)
    Push { image: String },
    /// List local images
    Ls,
    /// Everything recorded about an image: OS version, edition, build, source, tool versions
    Info { image: String },
    /// Delete a local image
    Rm { image: String },
    /// Recapture the RAM snapshot new VMs resume from (build and pull do this)
    Snapshot { image: String },
}

fn main() {
    ensure_path();
    if let Err(e) = instance::migrate_legacy_images() {
        log!("upgrading the image layout failed: {e:#}");
    }
    if let Err(e) = run(Cli::parse()) {
        log!("FAIL: {e:#}");
        std::process::exit(1);
    }
}

fn run(cli: Cli) -> Result<()> {
    match cli.cmd {
        Cmd::Create {
            image,
            name,
            memory,
            cpus,
        } => out(ops::create(
            &Image::resolve(&image)?,
            name.as_deref(),
            memory,
            cpus,
        )),
        Cmd::List => out(ops::list_table()),
        Cmd::Info { name } => out(Ok(ops::info(&Instance::load(&name)?))),
        Cmd::Start { name } => out(ops::boot(&Instance::load(&name)?)),
        Cmd::Stop { name, all } => match (name, all) {
            (Some(name), false) => out(ops::stop(&Instance::load(&name)?)),
            (None, true) => out(ops::stop_all()),
            _ => anyhow::bail!("name a VM, or pass --all"),
        },
        Cmd::Reset { name } => out(ops::reset(&Instance::load(&name)?)),
        Cmd::Rm { name } => out(ops::delete(&Instance::load(&name)?)),
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
        } => out(ops::forward(&Instance::load(&name)?, guest_port, host_port)),
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
            ImageCmd::Rm { image: i } => out(image::remove(&i.parse()?)),
            ImageCmd::Snapshot { image: i } => image::snapshot(&i.parse::<Image>()?),
        },
        Cmd::Mcp => tokio::runtime::Runtime::new()?.block_on(mcp::serve()),
        Cmd::McpInstall { clients } => setup::mcp_install(&clients),
        Cmd::McpUninstall { clients } => setup::mcp_uninstall(&clients),
        Cmd::Clean { dry_run } => out(setup::clean(dry_run)),
        Cmd::Uninstall { keep_data, yes } => setup::uninstall(keep_data, yes),
        Cmd::Doctor => {
            if !setup::doctor()? {
                std::process::exit(1);
            }
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
            "exactly one side must be <vm>:<path>, e.g. agentpc cp ./file ubuntu-1:/tmp/"
        ),
    }
}

fn out(r: Result<String>) -> Result<()> {
    println!("{}", r?.trim_end());
    Ok(())
}
