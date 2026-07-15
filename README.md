# git-stack

Manage **stacks** of dependent branches and their **numbered pull requests**.

A *stack* is an ordered series of branches where branch *N* is built on top of
branch *N-1*. The stack therefore has a well-defined merge order: the bottom
branch merges into trunk first, then the next, and so on. `git-stack` tracks
that order, keeps the whole stack rebased on trunk, and opens one PR per branch
— each targeting the branch below it, titled `[k/n]`, and cross-linked with a
shared stack map.

```
◉ ui        #12 [OPEN]   ← current
◯ service   #11 [OPEN]
◯ api       #10 [OPEN]
┴
  main (trunk)
```

## Install

```sh
./install.sh          # cargo install + man page
git stack --version
```

`install.sh` runs `cargo install --path .` and then writes the man page to a
directory on your `MANPATH` (`cargo install` only ever installs *binaries*, so
the man page is placed separately). Afterwards both `man git-stack` and
`git stack --help` work — git routes `git stack --help` to `man git-stack`.

Binary only, no man page:

```sh
cargo install --path .
```

Because the binary is named `git-stack`, git dispatches `git stack …` to it
automatically (git's standard subcommand mechanism). It relies on `git` and, for
PRs, the authenticated [`gh`](https://cli.github.com) CLI.

> The man page is generated from the CLI definition itself (`clap_mangen`), so it
> never drifts. Regenerate manually with `git-stack man --dir <man1-dir>`.

## Workflow

```sh
git stack init                 # record the trunk (auto-detects main/master)

git checkout main
git stack create api           # new branch on trunk; make commits
git stack create service       # new branch on top of `api`; make commits
git stack create ui            # new branch on top of `service`; make commits

git stack status               # view the stack and PR states
git stack down / up            # walk down/up the stack (aliases: prev/next)

git stack submit               # push all branches + open/update numbered PRs
# ... trunk moves on, a teammate pushes to a branch, or you amend a lower one ...
git stack sync                 # pull remote commits, restack onto trunk, push (with lease)
git stack submit               # refresh the PRs
```

## Commands

| Command | Description |
|---|---|
| `git stack init [--trunk <b>]` | Record the trunk branch for this repo. |
| `git stack create <name>` | Create `<name>` on top of the current branch and track it. |
| `git stack split` | Split the current branch's commits into a stack (editor assigns commits to branches). |
| `git stack track [--parent <b>]` | Adopt the current branch into a stack (parent defaults to trunk). |
| `git stack untrack` | Forget the current branch's stack metadata. |
| `git stack describe [-m <text>]` | Describe what the current branch/PR is about; becomes the PR body (opens `$EDITOR` without `-m`). |
| `git stack status` (`ls`, `list`) | Show the stack tree with PR numbers/states. |
| `git stack up` / `down` (`next`/`prev`) | Check out the child / parent branch. |
| `git stack commit [-m <msg>]` | Make a **new** commit on the current branch, then restack all descendants onto the new tip. |
| `git stack amend` | Fold **staged** changes into the current commit and update every descendant. |
| `git stack reword [<commit>]` | Rewrite a commit message and update descendants (defaults to HEAD). |
| `git stack restack` | Restack the current branch's descendants onto its tip. |
| `git stack hooks install` / `uninstall` | Make plain `git commit`/amend auto-restack descendants. |
| `git stack sync [--no-push]` | Pull remote commits into stack branches, restack onto the latest trunk, and push back with `--force-with-lease`. |
| `git stack submit [--draft]` (`push`) | Push the current stack line and open/update its numbered PRs. |

### Editing a branch in the middle of a stack

When you change a branch that has descendants, git-stack propagates the change
up the stack automatically — forks included — using two engines depending on
what you're doing:

| You want to… | Command | Engine | On conflict |
|---|---|---|---|
| Add **new** work | `git stack commit` | `git replay --contained` (whole subtree, one atomic ref update) | conflict markers are **persisted** into the committed files so it always finishes, and the affected branches are flagged with a loud warning |
| **Amend** a commit | `git stack amend` | `git history fixup` (atomic, worktree-free) | **aborts cleanly**, nothing changes, loud warning — `git history` cannot leave markers |
| **Reword** a message | `git stack reword` | `git history reword` | aborts cleanly |

Branches left holding persisted conflict markers are shown with `⚠ conflict
markers` in `git stack status`. Search for `<<<<<<<`, resolve, and commit.

Prefer plain `git commit`? Run `git stack hooks install` and a post-commit /
post-rewrite hook will call `git stack restack` for you (the hooks are guarded
against recursion and no-op off a stack).

> Note: `git stack --help` and `git stack <cmd> --help` are intercepted by git to
> open a man page (`man git-stack`). Install via `./install.sh` so that page
> exists; otherwise use `git stack help` or `git-stack --help` for inline help.

## How it works

State lives in the repository's own git config (nothing outside git):

| Key | Meaning |
|---|---|
| `stack.trunk` | Trunk branch name. |
| `stack.remote` | Remote to push/fetch (default `origin`). |
| `branch.<n>.stackParent` | Parent branch of `<n>`. |
| `branch.<n>.stackParentSha` | Parent tip when `<n>` was last based — the rebase anchor used by `sync`. |
| `branch.<n>.stackPr` | Cached PR number. |
| `branch.<n>.stackDescription` | PR body text set by `git stack describe`. |

(Conflict-marker state is **not** stored — `status` detects `<<<<<<<` in each
branch's tip live, so it can never report a stale warning.)

Branches form a forest under trunk via parent pointers; a *stack line* is the
linear chain from trunk to a leaf. `submit` pushes bottom-first (so each PR's
base exists), points each PR at the branch below it, and writes the `[k/n]`
titles. Every PR body gets a formatted, linked **stack list prepended** to the
top, followed by that branch's `describe` text under a divider:

```markdown
### 📚 Stacked PR · 2 of 3

Merge in order, bottom to top:

1. [#10 `api`](…/pull/10) → `main`
2. **[#11 `service`](…/pull/11) → `api`**  👈 **this PR**
3. [#12 `ui`](…/pull/12) → `service`

---

<your `git stack describe` text>
```

The list is bounded by hidden markers, so re-running `submit` re-renders it in
place (idempotent) without disturbing your description.

See [DESIGN.md](DESIGN.md) for the full design.

## Claude skill

[`skills/using-git-stack/SKILL.md`](skills/using-git-stack/SKILL.md) is a Claude
Code skill (CipherPowers format) that teaches Claude to drive `git stack`
confidently — the mental model, which command to reach for, conflict handling,
and worked examples. Copy it into your plugin's `skills/` directory to enable it.

## Development

```sh
cargo test      # unit tests + integration tests against throwaway repos
cargo build
```
