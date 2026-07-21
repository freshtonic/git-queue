//! `git queue` — manage queues of dependent branches and their numbered PRs.

mod commands;
mod gh;
mod git;
mod ident;
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
    /// The status tree with each branch's commits (and their Queued-Commit-Ids) shown.
    Log,
    /// Split the current branch's commits into a queue of branches.
    Split {
        /// If the original branch isn't reused as a segment, delete it without asking.
        #[arg(long)]
        delete_original: bool,
    },
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
        /// Stamp Queued-Commit-Ids onto existing commits without asking (rewrites them).
        #[arg(long, conflicts_with = "no_stamp_ids")]
        stamp_ids: bool,
        /// Never stamp Queued-Commit-Ids onto existing commits.
        #[arg(long)]
        no_stamp_ids: bool,
        /// After adopting, open the split editor to divide the commits into
        /// multiple queued branches.
        #[arg(long)]
        split: bool,
        /// With --split: delete the original branch without asking if it isn't
        /// reused as a segment.
        #[arg(long, requires = "split")]
        delete_original: bool,
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
        /// Commit to reword: a revision or a Queued-Commit-Id (unique prefix ok).
        commit: Option<String>,
    },
    /// Move a commit (or inclusive <first>..<last> range) elsewhere in the queue.
    Move {
        /// The commit to move, or an inclusive range `<first>..<last>` of
        /// consecutive commits. Each may be a revision or a Queued-Commit-Id
        /// (unique prefix ok). Must be commits of this queue.
        commit: String,
        /// The queue commit the moved commits should directly follow — a
        /// revision or a Queued-Commit-Id. Pass the base branch's tip commit
        /// to move them to the front of the queue.
        #[arg(long)]
        new_parent: String,
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
    /// Internal: GIT_SEQUENCE_EDITOR for id stamping (marks picks as reword).
    #[command(hide = true, name = "stamp-todo")]
    StampTodo {
        /// Path to the rebase todo file (passed by git).
        file: PathBuf,
    },
    /// Internal: commit-msg hook adding a `Queued-Commit-Id` trailer on queue branches.
    #[command(hide = true, name = "add-queue-id")]
    AddQueueId {
        /// Path to the commit-message file (passed by git).
        file: PathBuf,
    },
    /// Internal: GIT_SEQUENCE_EDITOR for `git queue move` (rewrites a rebase todo).
    #[command(hide = true, name = "reorder-todo")]
    ReorderTodo {
        /// Path to the rebase todo file (passed by git).
        file: PathBuf,
    },
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
        Command::Log => commands::log(),
        Command::Split { delete_original } => commands::split(delete_original),
        Command::Describe { message } => commands::describe(message),
        Command::Track {
            parent,
            stamp_ids,
            no_stamp_ids,
            split,
            delete_original,
        } => commands::track(parent, stamp_ids, no_stamp_ids, split, delete_original),
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
        Command::Move { commit, new_parent } => commands::move_commits(&commit, &new_parent),
        Command::StampTodo { file } => commands::stamp_todo(&file),
        Command::AddQueueId { file } => commands::add_queue_id(&file),
        Command::ReorderTodo { file } => commands::reorder_todo(&file),
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
