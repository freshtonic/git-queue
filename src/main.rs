//! `git stack` — manage stacks of dependent branches and their numbered PRs.

mod commands;
mod gh;
mod git;
mod meta;
mod render;
mod stack;

use clap::{CommandFactory, Parser, Subcommand};
use std::path::PathBuf;

#[derive(Parser)]
#[command(
    name = "git-stack",
    bin_name = "git stack",
    version,
    about = "Manage stacks of dependent branches and their numbered pull requests",
    long_about = "A stack is an ordered series of branches where branch N is built on top of \
                  branch N-1. git-stack tracks that order, keeps the stack rebased on trunk, \
                  and opens numbered, cross-linked pull requests — one per branch."
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Configure the trunk branch for this repository.
    Init {
        /// Trunk branch (defaults to main/master if present).
        #[arg(long)]
        trunk: Option<String>,
    },
    /// Create a new branch stacked on top of the current one.
    Create {
        /// Name of the new branch.
        name: String,
    },
    /// Show the current stack and its PR status.
    #[command(visible_aliases = ["ls", "list"])]
    Status,
    /// Adopt the current branch into a stack.
    Track {
        /// Parent branch (defaults to trunk).
        #[arg(long)]
        parent: Option<String>,
    },
    /// Forget the current branch's stack metadata.
    Untrack,
    /// Check out the child branch (up the stack).
    #[command(visible_alias = "up")]
    Next,
    /// Check out the parent branch (down the stack).
    #[command(visible_alias = "down")]
    Prev,
    /// Rebase the whole stack onto the latest trunk.
    Sync,
    /// Push the stack and open/update its numbered PRs.
    #[command(visible_alias = "push")]
    Submit {
        /// Open new PRs as drafts.
        #[arg(long)]
        draft: bool,
    },
    /// Generate the roff man page (used by install.sh; also enables `git stack --help`).
    #[command(hide = true)]
    Man {
        /// Directory to write `git-stack.1` into. Prints to stdout if omitted.
        #[arg(long)]
        dir: Option<PathBuf>,
    },
}

/// Render the man page from the clap definition and either write
/// `<dir>/git-stack.1` or print it to stdout.
fn generate_man(dir: Option<PathBuf>) -> anyhow::Result<()> {
    use std::io::Write;
    let man = clap_mangen::Man::new(Cli::command());
    let mut buffer: Vec<u8> = Vec::new();
    man.render(&mut buffer)?;
    match dir {
        Some(dir) => {
            std::fs::create_dir_all(&dir)?;
            let path = dir.join("git-stack.1");
            std::fs::write(&path, &buffer)?;
            eprintln!("Wrote {}", path.display());
        }
        None => std::io::stdout().write_all(&buffer)?,
    }
    Ok(())
}

fn main() {
    let cli = Cli::parse();
    let result = match cli.command {
        Command::Init { trunk } => commands::init(trunk),
        Command::Create { name } => commands::create(&name),
        Command::Status => commands::status(),
        Command::Track { parent } => commands::track(parent),
        Command::Untrack => commands::untrack(),
        Command::Next => commands::next(),
        Command::Prev => commands::prev(),
        Command::Sync => commands::sync(),
        Command::Submit { draft } => commands::submit(draft),
        Command::Man { dir } => generate_man(dir),
    };
    if let Err(e) = result {
        eprintln!("error: {e:#}");
        std::process::exit(1);
    }
}
