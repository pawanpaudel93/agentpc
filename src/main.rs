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

use instance::{Instance, Os};

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
    Create { os: String, name: Option<String> },
    /// List VMs and images
    List,
    /// Viewer URL, SSH and VNC details
    Info { name: String },
    /// Boot a stopped VM
    Start { name: String },
    /// Shut a VM down cleanly (its disk is kept)
    Stop { name: String },
    /// Discard all changes: back to a fresh copy of the image
    Reset { name: String },
    /// Delete a VM and its disk
    Rm { name: String },
    /// Shell into a VM, or run a command (PowerShell on Windows, bash on Ubuntu)
    Ssh {
        name: String,
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        command: Vec<String>,
    },
    /// Save a PNG screenshot
    Screenshot { name: String, out: Option<PathBuf> },
    /// Manage images (the installed OS every VM is cloned from)
    #[command(subcommand)]
    Image(ImageCmd),
    /// Run the MCP server on stdio (what agents launch)
    Mcp,
    /// Register the MCP server with installed agents (claude, codex, cursor, gemini, vscode)
    McpInstall { clients: Vec<String> },
    /// Check prerequisites
    Doctor,
    #[command(name = "__viewer", hide = true)]
    Viewer,
}

#[derive(Subcommand)]
enum ImageCmd {
    /// Build an image locally by installing the OS (Ubuntu ~3 min, Windows ~12 min)
    Build {
        os: String,
        /// Windows 11 ARM64 ISO (default: $WIN_ISO or ~/Downloads/*A64FRE*.iso)
        #[arg(long)]
        iso: Option<PathBuf>,
    },
    /// Download a published image (Ubuntu only; Windows can't be redistributed)
    Pull {
        os: String,
        #[arg(long, default_value = "latest")]
        tag: String,
    },
    /// Publish the local image to the registry (maintainers; needs `oras login`)
    Push {
        os: String,
        #[arg(long, default_value = "latest")]
        tag: String,
    },
    /// List local images
    Ls,
    /// Everything recorded about an image: OS version, edition, build, source, tool versions
    Info { os: String },
    /// Delete a local image
    Rm { os: String },
    /// Recapture the RAM snapshot new VMs resume from (build and pull do this)
    Snapshot { os: String },
}

fn main() {
    if let Err(e) = run(Cli::parse()) {
        log!("FAIL: {e:#}");
        std::process::exit(1);
    }
}

fn run(cli: Cli) -> Result<()> {
    match cli.cmd {
        Cmd::Create { os, name } => out(ops::create(os.parse::<Os>()?, name.as_deref())),
        Cmd::List => out(ops::list_table()),
        Cmd::Info { name } => out(Ok(ops::info(&Instance::load(&name)?))),
        Cmd::Start { name } => out(ops::boot(&Instance::load(&name)?)),
        Cmd::Stop { name } => out(ops::stop(&Instance::load(&name)?)),
        Cmd::Reset { name } => out(ops::reset(&Instance::load(&name)?)),
        Cmd::Rm { name } => out(ops::delete(&Instance::load(&name)?)),
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
        Cmd::Screenshot { name, out: path } => {
            let inst = Instance::load(&name)?;
            let path = path.unwrap_or_else(|| inst.dir.join("screen.png"));
            let path = std::path::absolute(path)?;
            qemu::screenshot(&inst, &path)?;
            println!("{}", path.display());
            Ok(())
        }
        Cmd::Image(cmd) => match cmd {
            ImageCmd::Build { os, iso } => image::build(os.parse()?, iso),
            ImageCmd::Pull { os, tag } => registry::pull(os.parse()?, &tag),
            ImageCmd::Push { os, tag } => registry::push(os.parse()?, &tag),
            ImageCmd::Ls => out(image::list()),
            ImageCmd::Info { os } => out(image::describe(os.parse()?)),
            ImageCmd::Rm { os } => out(image::remove(os.parse()?)),
            ImageCmd::Snapshot { os } => image::snapshot(os.parse()?),
        },
        Cmd::Mcp => tokio::runtime::Runtime::new()?.block_on(mcp::serve()),
        Cmd::McpInstall { clients } => setup::mcp_install(&clients),
        Cmd::Doctor => {
            if !setup::doctor()? {
                std::process::exit(1);
            }
            Ok(())
        }
        Cmd::Viewer => viewer::serve(),
    }
}

fn out(r: Result<String>) -> Result<()> {
    println!("{}", r?.trim_end());
    Ok(())
}
