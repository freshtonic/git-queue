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
    #[command(
        long_about = "Records which branch is trunk (`queue.trunk` in the repo's git config), auto-detecting `main` or `master` when `--trunk` is omitted. Trunk is the default merge target for new queues and the reference point for landed-work detection. Run once per repository; re-run any time to change it. Nothing else is written and no commits are touched."
    )]
    Init {
        /// Trunk branch (defaults to main/master if present).
        #[arg(long)]
        trunk: Option<String>,
    },
    /// Create a new branch queued after the current one.
    #[command(
        long_about = "Creates `<name>` at the current branch's tip (or at `--base <branch>`'s tip) and records it as the next branch of the queue. Extending a queue inherits its name; starting a new one asks for a name (or takes `--queue`). The front PR of a queue targets its base branch — which is how queues can be built on release or bugfix branches, not just trunk. Make commits, then `git queue submit` opens the numbered PRs."
    )]
    Create {
        /// Name of the new branch.
        name: String,
        /// Start the queue on this base branch instead of the current branch.
        #[arg(long)]
        base: Option<String>,
        /// Name for the queue when this starts a new one.
        #[arg(long)]
        queue: Option<String>,
    },
    /// Show the current queue and its PR status.
    #[command(
        long_about = "Read-only view of the current line: one row per branch, front of the queue at the bottom, with PR number and state, Stable-Commit-Id coverage (`id ✓`, or `id 2/3` when partial), persisted-conflict warnings, and a marker for the checked-out branch. Conflict markers are detected live from each tip, so the warning can never be stale."
    )]
    Status,
    /// List every queue in the repo, most recently touched first.
    #[command(visible_alias = "list")]
    #[command(
        long_about = "Lists every queue in the repository, most recently touched first — each with its branch roster, the first line of its description, and a marker for the queue you're on. Activity timestamps update whenever a queue operation runs, so the top entry is what you worked on last. Unnamed queues are listed with instructions to name them."
    )]
    Ls,
    /// Show or set the current queue's name.
    #[command(
        long_about = "Shows the current queue's name, or (re)names it. Every queue is named: the name appears in each PR's header, namespaces split branches (`queue/<name>/<segment>`), and keys the queue-level description and activity time. Naming records membership on every branch of the line, so branches that don't follow the `queue/<name>/…` convention still resolve."
    )]
    Name {
        /// New name for the queue (shows the current name when omitted).
        name: Option<String>,
    },
    /// The status tree with each branch's commits (and their Stable-Commit-Ids) shown.
    #[command(
        long_about = "`status` with one more level of depth: each branch's commits are listed beneath it, newest first, prefixed with the abbreviated Stable-Commit-Id (`(no id)` for unstamped commits). Those abbreviations are accepted by every command that takes a commit, so `log` is the natural way to find the argument for `move`, `checkout` or `reword`."
    )]
    Log,
    /// Detach HEAD on a queue commit (SHA or Stable-Commit-Id) to edit it in place.
    #[command(
        long_about = "Detaches HEAD on a commit of the current queue — named by SHA or Stable-Commit-Id — after validating membership and a clean stage. From there, edit and `git add`, then: plain `git commit` INSERTS a new commit right after it, while `git commit --amend` REVISES it (the message carries over, so its Stable-Commit-Id is preserved). Either way everything that followed rebases onto the new commit, automatically with hooks installed or via `git queue requeue`. HEAD stays detached at the edited commit so you can keep going; `git queue checkout <branch>` reattaches and ends the session."
    )]
    Checkout {
        /// A commit of the current queue, or one of its branches to reattach.
        commit: String,
    },
    /// Split the current branch's commits into a queue of branches.
    #[command(
        long_about = "Turns one branch with many commits into a queue of branches: an editor lists every commit prefixed by a branch name — edit the names, and consecutive commits sharing a name become one branch/PR, in file order. Branches are created as `queue/<name>/<segment>` at the existing commit boundaries (no commits are rewritten). If no segment reuses the original branch's name, the old ref is fully redundant and split offers to delete it (`--delete-original` skips the prompt)."
    )]
    Split {
        /// If the original branch isn't reused as a segment, delete it without asking.
        #[arg(long)]
        delete_original: bool,
        /// Queue name; segment branches are created as queue/<name>/<segment>.
        #[arg(long)]
        queue: Option<String>,
    },
    /// Describe the current QUEUE (the "About this queue" section of its PRs).
    #[command(
        long_about = "Sets the QUEUE's description — the \"About this queue\" section rendered into every PR of the queue on the next submit/sync. Use it for the narrative that spans the whole queue: what the series achieves and how the pieces fit. Opens `$EDITOR` without `-m`; an empty message clears it."
    )]
    Describe {
        /// Description text (opens $EDITOR if omitted).
        #[arg(short = 'm', long)]
        message: Option<String>,
    },
    /// Describe the current BRANCH (the "About this branch" section of its PR).
    #[command(name = "describe-branch")]
    #[command(
        long_about = "Sets the current BRANCH's description — the \"About this branch\" section of its PR. Use it for what this one slice does. A hand-written body on a PR adopted from before the queue existed is imported here automatically rather than overwritten. Opens `$EDITOR` without `-m`; an empty message clears it."
    )]
    DescribeBranch {
        /// Description text (opens $EDITOR if omitted).
        #[arg(short = 'm', long)]
        message: Option<String>,
    },
    /// Adopt the current branch into a queue.
    #[command(
        long_about = "Adopts an existing branch into a queue: records its parent (trunk by default, or `--parent`), asks for a queue name when starting a new one, and offers to stamp Stable-Commit-Ids onto the adopted commits — asking first because stamping rewrites them (hashes change; an already-pushed branch will be force-pushed with lease on the next sync). `--stamp-ids`/`--no-stamp-ids` decide non-interactively, and `--split` continues straight into the split editor to divide the adopted commits into multiple branches."
    )]
    Track {
        /// Parent branch (defaults to trunk).
        #[arg(long)]
        parent: Option<String>,
        /// Stamp Stable-Commit-Ids onto existing commits without asking (rewrites them).
        #[arg(long, conflicts_with = "no_stamp_ids")]
        stamp_ids: bool,
        /// Never stamp Stable-Commit-Ids onto existing commits.
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
        /// Name for the queue when this adoption starts a new one.
        #[arg(long)]
        queue: Option<String>,
    },
    /// Forget the current branch's queue metadata.
    #[command(
        long_about = "Forgets the current branch's queue metadata (parent, anchor, cached PR number, descriptions, membership). The branch and its commits are untouched — this only removes it from the queue's structure."
    )]
    Untrack,
    /// Check out the child branch (toward the back of the queue).
    #[command(visible_alias = "up")]
    #[command(
        long_about = "Checks out the child branch — one step toward the back of the queue. Errors helpfully at the top or at a fork (listing the children so you can pick one)."
    )]
    Next,
    /// Check out the parent branch (toward the front of the queue).
    #[command(visible_alias = "down")]
    #[command(
        long_about = "Checks out the parent branch — one step toward the front of the queue."
    )]
    Prev,
    /// Make a new commit on the current branch and requeue its descendants.
    #[command(
        long_about = "Commits like `git commit`, then requeues every descendant branch onto the new tip in one atomic pass (engine: `git replay`), so the branches behind yours never go stale. Stamps a Stable-Commit-Id if the commit-msg hook didn't. With hooks installed, plain `git commit` behaves the same — this command exists for hookless repositories and for scripting."
    )]
    Commit {
        /// Commit message (opens the editor if omitted).
        #[arg(short = 'm', long)]
        message: Option<String>,
    },
    /// Fold STAGED changes into the current commit and update all descendants.
    #[command(
        long_about = "Folds the STAGED changes into the current branch's tip commit and updates every descendant, atomically (engine: `git history fixup`): if propagating would conflict with a descendant, it aborts and changes nothing — it cannot leave conflict markers. This is the everyday tool for addressing review feedback on a PR that has others queued behind it. The commit message, and with it the Stable-Commit-Id, is preserved."
    )]
    Amend,
    /// Rewrite a commit message and update all descendants (defaults to HEAD).
    #[command(
        long_about = "Rewrites a commit's message — HEAD by default, or any queue commit named by SHA or Stable-Commit-Id — and updates every descendant (engine: `git history reword`; atomic, aborts cleanly on conflict). Content is untouched."
    )]
    Reword {
        /// Commit to reword: a revision or a Stable-Commit-Id (unique prefix ok).
        commit: Option<String>,
    },
    /// Move a commit (or inclusive <first>..<last> range) elsewhere in the queue.
    #[command(
        long_about = "Relocates a commit, or an inclusive `<first>..<last>` range of consecutive commits, to directly follow `--new-parent` — reordering within one PR or moving work to a different PR (the commits join the branch segment their new parent belongs to). All arguments accept SHAs or Stable-Commit-Ids. The whole line is rewritten in one `git rebase --update-refs` pass: branch refs ride along, conflicts are persisted as markers and flagged, and passing the base branch's tip as `--new-parent` moves commits to the very front of the queue."
    )]
    Move {
        /// The commit to move, or an inclusive range `<first>..<last>` of
        /// consecutive commits. Each may be a revision or a Stable-Commit-Id
        /// (unique prefix ok). Must be commits of this queue.
        commit: String,
        /// The queue commit the moved commits should directly follow — a
        /// revision or a Stable-Commit-Id. Pass the base branch's tip commit
        /// to move them to the front of the queue.
        #[arg(long)]
        new_parent: String,
    },
    /// Requeue the current branch's descendants onto its tip.
    #[command(visible_alias = "restack")]
    #[command(
        long_about = "The repair primitive: makes every descendant of the current branch consistent with its tip — nothing else. No network, no PR edits, no pruning. It is what the hooks run after a plain `git commit`/`--amend` (`--auto` stays silent when there is nothing to do), and the only command that reintegrates a `git queue checkout` editing session when hooks aren't installed. Reach for it manually after hand-made history surgery (cherry-picks, resets) leaves the branches above you stale."
    )]
    Requeue {
        /// Quiet on no-op / non-queue branches (used by hooks).
        #[arg(long)]
        auto: bool,
    },
    /// Install or remove hooks that auto-requeue after plain commits.
    #[command(
        long_about = "Installs (or removes) three per-repository hooks: post-commit and post-rewrite auto-requeue descendants after plain `git commit`/`--amend` on a queue branch — making the plumbing invisible — and commit-msg stamps a Stable-Commit-Id trailer on every new queue commit. Guarded against recursion; no-ops on branches outside any queue."
    )]
    Hooks {
        #[command(subcommand)]
        action: HooksAction,
    },
    /// Pull remote commits, requeue onto the latest base branch, and push (with lease).
    #[command(
        long_about = "The converge-with-reality command. Fetches with `--prune`, fast-forwards each queue's base branch, drops branches whose work has landed (merged PRs, and squash-merges detected by Stable-Commit-Id), pulls genuinely new teammate commits (id correspondence guarantees your own rewrites are never re-applied over themselves), requeues the whole forest onto its bases, pushes everything back with `--force-with-lease`, and reconciles the PRs of every published queue — opening missing ones, reviving closed ones, refreshing bases, titles and queue maps. `--no-push` stops after the local requeue. It never pushes a branch that would make GitHub mislabel an open child PR as merged."
    )]
    Sync {
        /// Skip pushing the branches back to the remote.
        #[arg(long)]
        no_push: bool,
    },
    /// Push the queue and open/update its numbered PRs.
    #[command(visible_alias = "push")]
    #[command(
        long_about = "Publishes the current line: pushes every branch (front first, so bases exist), opens or revives its numbered PRs, and rewrites each PR's base, `[k/n]` title and body (queue map + About sections). Adopted PRs keep their hand-written titles (renumbered) and bodies. With the status gate enabled it also posts the red/green merge-order statuses. `--draft` opens new PRs as drafts."
    )]
    Submit {
        /// Open new PRs as drafts.
        #[arg(long)]
        draft: bool,
    },
    /// Close every open (non-merged) PR in the current queue.
    #[command(
        long_about = "Closes every open PR of the current queue without merging — for abandoning or restarting a published queue. Merged PRs, local branches and metadata are all left untouched."
    )]
    Yank,
    /// Report whether merge-order signalling is set up (read-only).
    #[command(
        long_about = "Read-only diagnosis of merge-order signalling: whether the status gate is enabled, and whether the GitHub CLI is ready. Changes nothing."
    )]
    Doctor,
    /// Enable merge-order signalling: submit posts a red/green commit status per PR.
    #[command(
        long_about = "Enables the advisory merge-order gate: from then on, submit/sync post a `git-queue/merge-order` commit status on every open PR — green ✓ on the PR at the front of the queue, red ✗ (\"merge PR #N first\", linking to the blocker) on the ones behind it. Advisory by design: every hard block GitHub offers lives on base-branch rules, which would also reject git-queue's own pushes to queue branches. PRs stay normal, reviewable, non-draft."
    )]
    Protect,
    /// Internal: GIT_SEQUENCE_EDITOR for id stamping (marks picks as reword).
    #[command(hide = true, name = "stamp-todo")]
    StampTodo {
        /// Path to the rebase todo file (passed by git).
        file: PathBuf,
    },
    /// Internal: commit-msg hook adding a `Stable-Commit-Id` trailer on queue branches.
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

/// Render the man page. clap_mangen produces the top-level page, whose
/// SUBCOMMANDS section references non-existent per-subcommand pages
/// (git-queue-init(1)); we replace it with a fully detailed COMMANDS
/// section — real `git queue <cmd>` names, the long description, and every
/// argument — generated from the same clap definitions.
fn generate_man(dir: Option<PathBuf>) -> anyhow::Result<()> {
    use std::io::Write;
    let cmd = Cli::command();
    let man = clap_mangen::Man::new(cmd.clone());
    let mut buffer: Vec<u8> = Vec::new();
    man.render(&mut buffer)?;
    let text = String::from_utf8(buffer)?;

    // Drop the auto-generated SUBCOMMANDS section (starts at .SH SUBCOMMANDS,
    // ends at the next .SH) and put the detailed section in its place.
    let text = match text.find(".SH SUBCOMMANDS") {
        Some(start) => {
            let rest = &text[start + 4..];
            let end = rest
                .find(".SH ")
                .map(|i| start + 4 + i)
                .unwrap_or(text.len());
            format!(
                "{}{}{}",
                &text[..start],
                commands_section(&cmd),
                &text[end..]
            )
        }
        None => text + &commands_section(&cmd),
    };

    match dir {
        Some(dir) => {
            std::fs::create_dir_all(&dir)?;
            let path = dir.join("git-queue.1");
            std::fs::write(&path, text)?;
            eprintln!("Wrote {}", path.display());
        }
        None => std::io::stdout().write_all(text.as_bytes())?,
    }
    Ok(())
}

fn roff(s: &str) -> String {
    s.replace('\\', "\\\\").replace('-', "\\-")
}

/// One detailed roff section per visible subcommand.
fn commands_section(cmd: &clap::Command) -> String {
    let mut out = String::from(".SH COMMANDS\n");
    for sub in cmd.get_subcommands().filter(|s| !s.is_hide_set()) {
        let mut synopsis = format!("git queue {}", sub.get_name());
        for a in sub.get_arguments().filter(|a| a.get_id() != "help") {
            let piece = if a.is_positional() {
                let v = a.get_id().to_string().to_uppercase();
                if a.is_required_set() {
                    format!("<{v}>")
                } else {
                    format!("[{v}]")
                }
            } else {
                let long = a.get_long().map(|l| format!("--{l}")).unwrap_or_default();
                let val = if a.get_action().takes_values() {
                    format!(" <{}>", a.get_id().to_string().to_uppercase())
                } else {
                    String::new()
                };
                if a.is_required_set() {
                    format!("{long}{val}")
                } else {
                    format!("[{long}{val}]")
                }
            };
            synopsis.push(' ');
            synopsis.push_str(&piece);
        }
        out.push_str(&format!(".SS \\fB{}\\fR\n", roff(&synopsis)));
        let aliases: Vec<String> = sub.get_visible_aliases().map(str::to_string).collect();
        if !aliases.is_empty() {
            out.push_str(&format!("Alias: {}\n.PP\n", roff(&aliases.join(", "))));
        }
        let about = sub
            .get_long_about()
            .or_else(|| sub.get_about())
            .map(|s| s.to_string())
            .unwrap_or_default();
        out.push_str(&format!("{}\n", roff(&about)));
        for a in sub.get_arguments().filter(|a| a.get_id() != "help") {
            let name = if a.is_positional() {
                format!("<{}>", a.get_id().to_string().to_uppercase())
            } else {
                let val = if a.get_action().takes_values() {
                    format!(" <{}>", a.get_id().to_string().to_uppercase())
                } else {
                    String::new()
                };
                format!("--{}{val}", a.get_long().unwrap_or_default())
            };
            let req = if a.is_required_set() {
                " (required)"
            } else {
                " (optional)"
            };
            let help = a
                .get_long_help()
                .or_else(|| a.get_help())
                .map(|h| h.to_string())
                .unwrap_or_default();
            out.push_str(&format!(
                ".TP\n\\fB{}\\fR{}\n{}\n",
                roff(&name),
                req,
                roff(&help)
            ));
        }
    }
    out
}

/// Parse the CLI and run the selected subcommand. Exits the process on error.
/// Called by both binaries (`git-queue` and its alias `git-q`).
pub fn run() {
    let cli = Cli::parse();
    let result = match cli.command {
        Command::Init { trunk } => commands::init(trunk),
        Command::Create { name, base, queue } => {
            commands::create(&name, base.as_deref(), queue.as_deref())
        }
        Command::Status => commands::status(),
        Command::Ls => commands::ls(),
        Command::Name { name } => commands::name(name),
        Command::Log => commands::log(),
        Command::Checkout { commit } => commands::checkout(&commit),
        Command::Split {
            delete_original,
            queue,
        } => commands::split(delete_original, queue.as_deref()),
        Command::Describe { message } => commands::describe(message),
        Command::DescribeBranch { message } => commands::describe_branch(message),
        Command::Track {
            parent,
            stamp_ids,
            no_stamp_ids,
            split,
            delete_original,
            queue,
        } => commands::track(
            parent,
            stamp_ids,
            no_stamp_ids,
            split,
            delete_original,
            queue.as_deref(),
        ),
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
