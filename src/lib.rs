//! `git queue` — manage queues of dependent branches and their numbered PRs.

mod commands;
mod gh;
mod git;
mod meta;
mod queue;
mod render;
mod requeue;

use clap::{CommandFactory, Parser, Subcommand};
use std::path::PathBuf;

#[derive(Parser)]
#[command(
    name = "git-queue",
    bin_name = "git queue",
    version,
    about = "Manage queues of dependent branches and their numbered pull requests",
    long_about = "A PR queue is an ordered series of dependent branches: each branch builds on \
                  the one before it, and the PRs merge in FIFO order — front of the queue \
                  first. git-queue tracks that order, keeps the queue rebased on its base \
                  branch, and opens numbered, cross-linked pull requests — one per branch. \
                  Installed as both `git-queue` and `git-q`, so `git queue …` and `git q …` \
                  are equivalent."
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
    /// Create a new branch queued after the current one.
    Create {
        /// Name of the new branch.
        name: String,
        /// Start the queue on this base branch instead of the current branch.
        #[arg(long)]
        base: Option<String>,
    },
    /// Show the current queue and its PR status.
    #[command(visible_aliases = ["ls", "list"])]
    Status,
    /// Split the current branch's commits into a queue of branches.
    Split,
    /// Describe what the current branch/PR is about (becomes the PR body).
    Describe {
        /// Description text (opens $EDITOR if omitted).
        #[arg(short = 'm', long)]
        message: Option<String>,
    },
    /// Adopt the current branch into a queue.
    Track {
        /// Parent branch (defaults to trunk).
        #[arg(long)]
        parent: Option<String>,
    },
    /// Forget the current branch's queue metadata.
    Untrack,
    /// Check out the child branch (toward the back of the queue).
    #[command(visible_alias = "up")]
    Next,
    /// Check out the parent branch (toward the front of the queue).
    #[command(visible_alias = "down")]
    Prev,
    /// Make a new commit on the current branch and requeue its descendants.
    Commit {
        /// Commit message (opens the editor if omitted).
        #[arg(short = 'm', long)]
        message: Option<String>,
    },
    /// Fold STAGED changes into the current commit and update all descendants.
    Amend,
    /// Rewrite a commit message and update all descendants (defaults to HEAD).
    Reword {
        /// Commit to reword.
        commit: Option<String>,
    },
    /// Requeue the current branch's descendants onto its tip.
    #[command(visible_alias = "restack")]
    Requeue {
        /// Quiet on no-op / non-queue branches (used by hooks).
        #[arg(long)]
        auto: bool,
    },
    /// Install or remove hooks that auto-requeue after plain commits.
    Hooks {
        #[command(subcommand)]
        action: HooksAction,
    },
    /// Pull remote commits, requeue onto the latest base branch, and push (with lease).
    Sync {
        /// Skip pushing the branches back to the remote.
        #[arg(long)]
        no_push: bool,
    },
    /// Push the queue and open/update its numbered PRs.
    #[command(visible_alias = "push")]
    Submit {
        /// Open new PRs as drafts.
        #[arg(long)]
        draft: bool,
    },
    /// Close every open (non-merged) PR in the current queue.
    Yank,
    /// Report whether merge-order signalling is set up (read-only).
    Doctor,
    /// Enable merge-order signalling: submit posts a red/green commit status per PR.
    Protect,
    /// Generate the roff man page (used by install.sh; also enables `git queue --help`).
    #[command(hide = true)]
    Man {
        /// Directory to write `git-queue.1` into. Prints to stdout if omitted.
        #[arg(long)]
        dir: Option<PathBuf>,
    },
}

#[derive(Subcommand)]
enum HooksAction {
    /// Install the auto-requeue hooks.
    Install,
    /// Remove the auto-requeue hooks.
    Uninstall,
}

/// Render the man page from the clap definition and either write
/// `<dir>/git-queue.1` or print it to stdout.
fn generate_man(dir: Option<PathBuf>) -> anyhow::Result<()> {
    use std::io::Write;
    let man = clap_mangen::Man::new(Cli::command());
    let mut buffer: Vec<u8> = Vec::new();
    man.render(&mut buffer)?;
    match dir {
        Some(dir) => {
            std::fs::create_dir_all(&dir)?;
            let path = dir.join("git-queue.1");
            std::fs::write(&path, &buffer)?;
            eprintln!("Wrote {}", path.display());
        }
        None => std::io::stdout().write_all(&buffer)?,
    }
    Ok(())
}

/// Parse the CLI and run the selected subcommand. Exits the process on error.
/// Called by both binaries (`git-queue` and its alias `git-q`).
pub fn run() {
    let cli = Cli::parse();
    let result = match cli.command {
        Command::Init { trunk } => commands::init(trunk),
        Command::Create { name, base } => commands::create(&name, base.as_deref()),
        Command::Status => commands::status(),
        Command::Split => commands::split(),
        Command::Describe { message } => commands::describe(message),
        Command::Track { parent } => commands::track(parent),
        Command::Untrack => commands::untrack(),
        Command::Next => commands::next(),
        Command::Prev => commands::prev(),
        Command::Sync { no_push } => commands::sync(no_push),
        Command::Submit { draft } => commands::submit(draft),
        Command::Yank => commands::yank(),
        Command::Doctor => commands::doctor(),
        Command::Protect => commands::protect(),
        Command::Commit { message } => commands::commit(message),
        Command::Amend => commands::amend(),
        Command::Reword { commit } => commands::reword(commit),
        Command::Requeue { auto } => commands::requeue(auto),
        Command::Hooks { action } => match action {
            HooksAction::Install => commands::hooks_install(),
            HooksAction::Uninstall => commands::hooks_uninstall(),
        },
        Command::Man { dir } => generate_man(dir),
    };
    if let Err(e) = result {
        eprintln!("error: {e:#}");
        std::process::exit(1);
    }
}
