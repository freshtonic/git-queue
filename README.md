# git-queue

Manage **PR queues**: ordered series of dependent branches and their
**numbered pull requests**.

> ⚠️ **Alpha software — run at your own risk.** git-queue is young and moving
> fast. It rewrites branches and force-pushes (with lease) as part of normal
> operation. Expect rough edges, keep backups of work you can't afford to
> lose, and read command output before trusting it.

A *PR queue* is an ordered series of branches where branch *N* is built on
branch *N-1*. The queue has a well-defined FIFO merge order: the branch at the
front merges into the base first, then the next, and so on. `git-queue` tracks
that order, keeps the whole queue rebased on its base, and opens one PR per
branch — each targeting the branch before it, titled `[k/n]`, and cross-linked
with a shared queue map.

> **Why "queue"?** Partly because `git-stack` was taken — but mostly because
> the standard nomenclature is wrong. So-called "stacked" PRs don't pop
> last-in-first-out; they merge **first-in-first-out**, which is a queue. If
> you already think in stacked PRs, just mentally replace "queue" with
> "stack".

```
◉ ui        #12 [OPEN]   ← current
◯ service   #11 [OPEN]
◯ api       #10 [OPEN]
┴
  main (trunk)
```

## Install

```sh
git clone git@github.com:freshtonic/git-queue.git
cd git-queue
./install.sh          # cargo install (the binary) + install the man page
git queue --version
```

`install.sh` runs `cargo install --path .` and then writes the man page to a
directory on your `MANPATH` (`cargo install` only ever installs *binaries*, so
the man page is placed separately). Afterwards both `man git-queue` and
`git queue --help` work — git routes `git queue --help` to `man git-queue`
(use `git queue help` for the inline CLI help).

Binary only, no man page:

```sh
cargo install --path .
```

Two binaries are installed — `git-queue` and its alias `git-q` — so git
dispatches both `git queue …` and the shorter `git q …` to the same tool
(git's standard subcommand mechanism).

### Optional: auto-requeue hooks

To have a plain `git commit`/`git commit --amend` on a queue branch
automatically requeue its descendants, install the git hooks (per repository):

```sh
git queue hooks install     # writes post-commit / post-rewrite hooks
git queue hooks uninstall   # remove them
```

Without the hooks, use `git queue commit` / `git queue amend` explicitly.

### Requirements

- **git** — `git queue commit`/`sync` use `git replay` (git ≥ 2.44) and
  `git queue amend`/`reword` use `git history` (git ≥ 2.55). Other commands work
  on older git; the ones needing a feature will tell you if it's missing.
- **[`gh`](https://cli.github.com)** — the authenticated GitHub CLI, for
  `submit`/`yank` (PR management). Not needed for local queue operations.

> The man page is generated from the CLI definition itself (`clap_mangen`), so it
> never drifts. Regenerate manually with `git-queue man --dir <man1-dir>`.

## Workflow

```sh
git queue init                 # record the trunk (auto-detects main/master)

git checkout main
git queue create api           # new branch on trunk; make commits
git queue create service       # next in the queue, after `api`; make commits
git queue create ui            # next in the queue, after `service`; make commits

git queue status               # view the queue and PR states
git queue prev / next          # walk toward the front/back (aliases: down/up)

git queue submit               # push all branches + open/update numbered PRs
# ... trunk moves on, a teammate pushes to a branch, or you amend an earlier PR ...
git queue sync                 # pull remote commits, requeue onto the base, push (with lease)
git queue submit               # refresh the PRs
```

### Queues on any base branch

A queue doesn't have to start on trunk. Run `create` from any branch — a
release branch, a long-lived bugfix branch — and that branch becomes the
queue's **base**: the front PR targets it, `sync` keeps the queue rebased on
it, and `status` shows it. Or name the base explicitly with `--base`:

```sh
git checkout release-1.2 && git queue create fix-a   # base inferred
git queue create fix-a --base release-1.2            # base named explicitly
```

## Commands

| Command | Description |
|---|---|
| `git queue init [--trunk <b>]` | Record the trunk branch for this repo. |
| `git queue create <name> [--base <b>]` | Create `<name>` queued after the current branch (or on base `<b>`) and track it. |
| `git queue split` | Split the current branch's commits into a queue (editor assigns commits to branches). |
| `git queue track [--parent <b>] [--stamp-ids\|--no-stamp-ids]` | Adopt the current branch into a queue (parent defaults to trunk). Offers to stamp `Queued-Commit-Id`s onto the adopted commits — asks first, since that rewrites their hashes. |
| `git queue untrack` | Forget the current branch's queue metadata. |
| `git queue describe [-m <text>]` | Describe what the current branch/PR is about; becomes the PR body (opens `$EDITOR` without `-m`). |
| `git queue status` (`ls`, `list`) | Show the queue tree with PR numbers/states and `Queued-Commit-Id` coverage. |
| `git queue log` | The status tree with each branch's commits indented beneath it, newest first, each prefixed by its abbreviated `Queued-Commit-Id`. |
| `git queue up` / `down` (`next`/`prev`) | Check out the child / parent branch. |
| `git queue commit [-m <msg>]` | Make a **new** commit on the current branch, then requeue all descendants onto the new tip. |
| `git queue amend` | Fold **staged** changes into the current commit and update every descendant. |
| `git queue reword [<commit>]` | Rewrite a commit message and update descendants (defaults to HEAD). |
| `git queue move <c>[..<c>] --new-parent <c>` | Move a commit (or an inclusive range) elsewhere in the queue — within one PR or across PRs. Commits can be named by revision or `Queued-Commit-Id` (unique prefix ok). Everything after the removal and insertion points is requeued; conflicts persist as markers. |
| `git queue requeue` (`restack`) | Requeue the current branch's descendants onto its tip. |
| `git queue hooks install` / `uninstall` | Make plain `git commit`/amend auto-requeue descendants and stamp `Queued-Commit-Id` trailers on new queue commits. |
| `git queue sync [--no-push]` | Pull remote commits, drop branches whose PRs have merged (reparenting their children), requeue onto the latest base, push back with `--force-with-lease`, and reconcile the PRs of every published queue (open missing ones, revive closed ones, fix bases/titles/queue maps). |
| `git queue submit [--draft]` (`push`) | Push the current queue line and open/update its numbered PRs (revives a child PR GitHub closed when its base was deleted). |
| `git queue yank` | Close every open (non-merged) PR in the current queue. |
| `git queue protect` | Enable merge-order signalling (a red/green commit status per PR) for this repo. |
| `git queue doctor` | Report whether merge-order signalling is enabled (read-only). |

### Signalling merge order

Run `git queue protect` once to warn reviewers off merging PRs out of order. It
sets a local flag (`queue.gate = status`) — no GitHub setup, workflow, ruleset,
or admin rights required. With it on, `git queue submit` posts a
`git-queue/merge-order` commit status on every open PR in the queue: green ✓
("Ready — front of the queue") on the PR that merges next, red ✗ ("Do not merge
— merge PR #N first") on every PR behind it, with the status's *Details* link
pointing at the PR that must land first. As PRs land, `git queue sync` +
`git queue submit` promote the PR now at the front to green.

`git queue doctor` reports whether the gate is on. The gate is **advisory**: the
red ✗ appears in the PR's checks list, but the merge button still works. PRs
stay normal, reviewable, non-draft PRs.

> Why a commit status, not something that disables the merge button? Every hard
> block GitHub offers lives on the *base* branch's rules — and with
> base-chaining a non-front PR targets an intermediate queue branch, so a
> ruleset that gates the merge also rejects git-queue's pushes to that branch
> (and draft PRs, the one PR-level block, read socially as "not ready for
> review"). A commit status is the strongest signal that leaves PRs normal and
> pushes unblocked.

### Adopting existing PRs

A queue can form around work that already has a PR — open a PR by hand, later
`git queue track` the branch and `create` more on top. `sync` (and `submit`)
reconcile the whole line: missing PRs are opened, and an existing PR is
adopted rather than overwritten — its hand-written title is kept (just
numbered `[k/n]`) and its body is preserved below the queue map. A queue with
no PRs at all is never auto-published; run `git queue submit` for that.

### Change identity: the `Queued-Commit-Id` trailer

Commit SHAs are useless identifiers in a rewrite-heavy workflow — every amend,
move and requeue mints new ones. git-queue therefore gives each *change* a
stable identity: a `Queued-Commit-Id:` trailer in the commit message (the same idea as
Gerrit's `Change-Id`), minted once and carried by git itself through every
rebase, cherry-pick, replay and amend.

- `git queue commit` stamps one automatically, and `git queue hooks install`
  adds a `commit-msg` hook so plain `git commit` on a queue branch does too.
  Commits off the queue are never touched.
- `git queue track` offers to stamp the branch's existing commits (with a
  confirmation — stamping rewrites those commits, so their hashes change and
  an already-pushed branch will be force-pushed on the next sync/submit).
  Use `--stamp-ids` / `--no-stamp-ids` to decide non-interactively.
- `git queue status` shows coverage per branch: `id ✓` when every commit
  carries one, `id 2/3` when only some do (nothing is shown for queues that
  haven't adopted ids).
- `sync` uses id correspondence to tell teammate work apart from stale copies
  of your own rewritten commits: only genuinely new commits are pulled, so a
  local amend or move can never conflict with its own pre-rewrite self on the
  remote. Id-less commits fall back to patch-equivalence.
- `sync` also drops branches whose every id already appears on trunk — which
  detects **squash-merges**, where SHAs and patch-ids are destroyed but
  GitHub's default squash message preserves the constituent trailers.

Anywhere a command takes a commit — `move` (including ranges and
`--new-parent`) and `reword` — you can pass a `Queued-Commit-Id` instead of a
revision: the full id or any unique prefix, exactly as `git queue log` shows
them.

Ids are optional and incremental: old commits without them keep working via
the previous heuristics, and new commits pick them up as they're made.

### Landing a queue

PRs merge in **FIFO order** — the oldest PR (`#1`, at the front of the queue)
merges first, then the next, and so on. After a PR lands, run `git queue sync` —
it detects the merged PR, reparents the branches behind it onto the base, and
rebases them; then `git queue submit` retargets their PR bases and refreshes the
queue list. Each PR body shows live approval/merge-state emojis (✅/♻️/⏳ while open,
🟣/⚫ once merged/closed) as of the last submit.

Deleting the merged branch is handled too: if a merge deletes the branch (via
`--delete-branch` or the repo's auto-delete setting), `sync` reparents its
orphaned children onto the queue's base, and — because GitHub closes a PR whose base branch
was deleted — `submit` revives that child PR (reopening it, or opening a fresh
one retargeted to trunk).

### Editing a branch in the middle of a queue

When you change a branch that has descendants, git-queue propagates the change
through the queue automatically — forks included — using two engines depending on
what you're doing:

| You want to… | Command | Engine | On conflict |
|---|---|---|---|
| Add **new** work | `git queue commit` | `git replay --contained` (whole subtree, one atomic ref update) | conflict markers are **persisted** into the committed files so it always finishes, and the affected branches are flagged with a loud warning |
| **Amend** a commit | `git queue amend` | `git history fixup` (atomic, worktree-free) | **aborts cleanly**, nothing changes, loud warning — `git history` cannot leave markers |
| **Reword** a message | `git queue reword` | `git history reword` | aborts cleanly |
| **Move** a commit / range | `git queue move <c>[..<c>] --new-parent <c>` | `git rebase -i --update-refs` with a scripted todo (whole line rewritten, branch refs ride along) | conflict markers are **persisted** and the affected branches flagged |

Branches left holding persisted conflict markers are shown with `⚠ conflict
markers` in `git queue status`. Search for `<<<<<<<`, resolve, and commit.

Prefer plain `git commit`? Run `git queue hooks install` and a post-commit /
post-rewrite hook will call `git queue requeue` for you (the hooks are guarded
against recursion and no-op off a queue).

> Note: `git queue --help` and `git queue <cmd> --help` are intercepted by git to
> open a man page (`man git-queue`). Install via `./install.sh` so that page
> exists; otherwise use `git queue help` or `git-queue --help` for inline help.

## How it works

State lives in the repository's own git config (nothing outside git):

| Key | Meaning |
|---|---|
| `queue.trunk` | Trunk branch name. |
| `queue.remote` | Remote to push/fetch (default `origin`). |
| `branch.<n>.queueParent` | Parent branch of `<n>`. |
| `branch.<n>.queueParentSha` | Parent tip when `<n>` was last based — the rebase anchor used by `sync`. |
| `branch.<n>.queuePr` | Cached PR number. |
| `branch.<n>.queueDescription` | PR body text set by `git queue describe`. |
| `queue.gate` | `status` once `git queue protect` enables merge-order signalling. |

(Conflict-marker state is **not** stored — `status` detects `<<<<<<<` in each
branch's tip live, so it can never report a stale warning.)

Branches form a forest under trunk via parent pointers; a *queue line* is the
linear chain from its base to a leaf. `submit` pushes front-first (so each PR's
base exists), points each PR at the branch below it, and writes the `[k/n]`
titles. Every PR body gets a formatted, linked **queue list prepended** to the
top, followed by that branch's `describe` text under a divider:

```markdown
### 📚 Queued PR · 2 of 3

Part of a queue. The PRs merge in FIFO order — the numbered order below, #1 first.

1. 🟣 [#10 `api`](…/pull/10) → `main`
2. ♻️🟢 **[#11 `service`](…/pull/11) → `api`**  👈 **this PR**
3. ⏳🟢 [#12 `ui`](…/pull/12) → `service`

---

<your `git queue describe` text>
```

The list is bounded by hidden markers, so re-running `submit` re-renders it in
place (idempotent) without disturbing your description.

See [DESIGN.md](DESIGN.md) for the full design.

## Claude skill

[`skills/using-git-queue/SKILL.md`](skills/using-git-queue/SKILL.md) is a Claude
Code skill (CipherPowers format) that teaches Claude to drive `git queue`
confidently — the mental model, which command to reach for, conflict handling,
and worked examples. Copy it into your plugin's `skills/` directory to enable it.

## Development

```sh
cargo test      # unit tests + integration tests against throwaway repos
cargo build
```
