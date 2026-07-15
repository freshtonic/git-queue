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
# ... trunk moves on, or you amend a lower branch ...
git stack sync                 # rebase the whole stack onto the latest trunk
git stack submit               # force-push and refresh the PRs
```

## Commands

| Command | Description |
|---|---|
| `git stack init [--trunk <b>]` | Record the trunk branch for this repo. |
| `git stack create <name>` | Create `<name>` on top of the current branch and track it. |
| `git stack track [--parent <b>]` | Adopt the current branch into a stack (parent defaults to trunk). |
| `git stack untrack` | Forget the current branch's stack metadata. |
| `git stack status` (`ls`, `list`) | Show the stack tree with PR numbers/states. |
| `git stack up` / `down` (`next`/`prev`) | Check out the child / parent branch. |
| `git stack sync` | Fetch and rebase every tracked branch onto the latest trunk. |
| `git stack submit [--draft]` (`push`) | Push the current stack line and open/update its numbered PRs. |

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

Branches form a forest under trunk via parent pointers; a *stack line* is the
linear chain from trunk to a leaf. `sync` restacks bottom-up with
`git rebase --onto <new-parent-tip> <old-anchor> <branch>`, so only each
branch's own commits are replayed. `submit` pushes bottom-first (so each PR's
base exists), points each PR at the branch below it, and writes the `[k/n]`
titles and shared navigation block.

See [DESIGN.md](DESIGN.md) for the full design.

## Development

```sh
cargo test      # unit tests + integration tests against throwaway repos
cargo build
```
