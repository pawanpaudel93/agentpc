//! agentpc: disposable Windows 11 / Ubuntu desktop VMs for AI agents on Apple Silicon.

mod bake;
mod instance;
mod mcp;
mod ops;
mod qemu;
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
    /// Install an OS once into a read-only golden image
    Bake {
        os: String,
        /// Windows 11 ARM64 ISO (default: $WIN_ISO or ~/Downloads/*A64FRE*.iso)
        #[arg(long)]
        iso: Option<PathBuf>,
    },
    /// Capture a running-desktop snapshot so new clones resume in ~1 s (bake does this)
    Snapshot {
        os: String,
    },
    /// Clone the golden image and boot it (default name: <os>-<n>)
    New {
        os: String,
        name: Option<String>,
    },
    /// Instances and golden images
    List,
    /// Viewer URL, SSH and VNC details
    Info {
        name: String,
    },
    Start {
        name: String,
    },
    Stop {
        name: String,
    },
    /// Discard all changes: back to the golden image
    Reset {
        name: String,
    },
    /// Stop and delete an instance
    Rm {
        name: String,
    },
    /// Shell into an instance, or run a command (PowerShell on Windows, bash on Ubuntu)
    Ssh {
        name: String,
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        command: Vec<String>,
    },
    /// Save a PNG screenshot
    Screen {
        name: String,
        out: Option<PathBuf>,
    },
    /// Run the MCP server on stdio (what agents launch)
    Mcp,
    /// Register the MCP server with installed agents (claude, codex, cursor, gemini, vscode)
    McpInstall {
        clients: Vec<String>,
    },
    /// Check prerequisites
    Doctor,
    #[command(name = "__viewer", hide = true)]
    Viewer,
}

fn main() {
    if let Err(e) = run(Cli::parse()) {
        log!("FAIL: {e:#}");
        std::process::exit(1);
    }
}

fn run(cli: Cli) -> Result<()> {
    match cli.cmd {
        Cmd::Bake { os, iso } => bake::bake(os.parse()?, iso),
        Cmd::Snapshot { os } => bake::snapshot(os.parse()?),
        Cmd::New { os, name } => out(ops::create(os.parse::<Os>()?, name.as_deref())),
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
        Cmd::Screen { name, out: path } => {
            let inst = Instance::load(&name)?;
            let path = path.unwrap_or_else(|| inst.dir.join("screen.png"));
            let path = std::path::absolute(path)?;
            qemu::screenshot(&inst, &path)?;
            println!("{}", path.display());
            Ok(())
        }
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
