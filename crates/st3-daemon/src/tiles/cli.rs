//! `st tiles`: run an agent's tools in its tile. Root runs these; a tile never does.

use std::ffi::OsString;
use std::path::PathBuf;

use anyhow::Result;
use clap::{Args, Subcommand};

use crate::tile_registry::DEFAULT_REGISTRY;

#[derive(Args, Debug)]
pub struct TilesArgs {
    #[command(subcommand)]
    command: TilesCommand,
}

#[derive(Subcommand, Debug)]
enum TilesCommand {
    /// Run a command in a tile and wait for it. Run as root.
    Run {
        #[arg(long, default_value = DEFAULT_REGISTRY)]
        registry: PathBuf,
        /// Give the command a terminal.
        #[arg(long)]
        pty: bool,
        /// The tile, as the registry names it.
        name: String,
        /// The command and its arguments, after `--`.
        #[arg(last = true, required = true, value_name = "COMMAND")]
        command: Vec<OsString>,
    },
    /// End every process in a tile. Run as root.
    Stop { name: String },
    /// Make a tile's directories, including the endpoint directory its supervisor serves,
    /// without starting it. Run as root, for example at boot.
    Prepare {
        #[arg(long, default_value = DEFAULT_REGISTRY)]
        registry: PathBuf,
        name: String,
    },
    /// End a tile, remove its state and directories, and give its user ID back. Run as root.
    /// Its registry entry stays; a later launch starts it afresh with a new user ID.
    Remove {
        #[arg(long, default_value = DEFAULT_REGISTRY)]
        registry: PathBuf,
        name: String,
    },
    /// Build the tile and run the command in it. The tile's unit runs this as root.
    #[command(hide = true)]
    Enter {
        #[arg(long, default_value = DEFAULT_REGISTRY)]
        registry: PathBuf,
        name: String,
        #[arg(last = true, required = true, value_name = "COMMAND")]
        command: Vec<OsString>,
    },
}

/// Run `st tiles`, returning the exit status. Runs before any async runtime or thread starts:
/// `enter` must be single-threaded to make its namespaces.
pub fn run(args: TilesArgs) -> Result<i32> {
    #[cfg(target_os = "linux")]
    {
        use super::launch;
        match args.command {
            TilesCommand::Run {
                registry,
                pty,
                name,
                command,
            } => launch::run(&registry, &name, pty, &command),
            TilesCommand::Stop { name } => launch::stop(&name).map(|()| 0),
            TilesCommand::Prepare { registry, name } => {
                launch::prepare(&registry, &name).map(|()| 0)
            }
            TilesCommand::Remove { registry, name } => launch::remove(&registry, &name).map(|()| 0),
            TilesCommand::Enter {
                registry,
                name,
                command,
            } => launch::enter(&registry, &name, &command),
        }
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = args;
        anyhow::bail!("tiles run on Linux; the macOS design is not built yet")
    }
}
