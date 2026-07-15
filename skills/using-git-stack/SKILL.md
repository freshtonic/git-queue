---
name: Using git-stack
description: Manage stacks of dependent branches and their numbered pull requests with the `git stack` subcommand
when_to_use: when work spans several dependent branches that should become separate, reviewable PRs; when editing a branch that has other branches stacked on top of it; when keeping a stack rebased on trunk and its PRs in sync; when a large branch should be split into a reviewable stack
version: 1.0.0
---

# Using git-stack

## Overview

`git stack` manages **stacks**: ordered series of branches where branch *N* is
built on top of branch *N-1*. The stack has a merge order — the bottom branch
merges into trunk first, then the next, and so on. git-stack tracks that order,
keeps the whole stack rebased, and opens one **numbered pull request per
branch**, each targeting the branch below it so every PR shows only its own
diff.

Prefer a stack over one giant branch whenever a change has separable parts
(types → service → API → UI): reviewers get small PRs, and each lands
independently in order.

**Mental model.** Each tracked branch stores its parent in git config
(`branch.<n>.stackParent`). Branches form a forest under `trunk`; a *stack line*
is the linear chain from trunk to a leaf. Nothing lives outside git — inspect
state with `git config --get-regexp '^branch\..*\.stack'`.

## The golden rule: don't hand-rebase a stack

When you change a branch that has descendants, **use the git-stack command for
it** so the change propagates to every branch above (forks included). Do not
`git rebase` stack branches by hand — you'll detach the children. The commands
below own that propagation.

## Choosing the right command

| Situation | Command | What happens |
|---|---|---|
| Start a new branch on top of the current one | `git stack create <name>` | Creates + tracks it |
| Add **new** work to a branch that has descendants | `git stack commit [-m …]` | New commit, then restacks all descendants |
| **Change an existing commit** on a mid-stack branch | `git add …` then `git stack amend` | Folds staged changes in, updates descendants |
| Fix a commit **message** | `git stack reword [<commit>]` | Rewrites message, updates descendants |
| Trunk moved / a teammate pushed / branches drifted | `git stack sync` | Pulls remote commits, restacks on trunk, pushes back |
| Open or refresh the PRs | `git stack submit` | Pushes + numbered, cross-linked PRs |
| Say what a PR is about | `git stack describe [-m …]` | Sets the PR body |
| One big branch → a stack | `git stack split` | Editor assigns commits to branches |
| Move around the stack | `git stack up` / `down`, `git stack status` | Navigate / view |

### commit vs amend — the key distinction

- **`git stack commit`** adds a *new* commit. Use it for genuinely new work.
  Engine: `git replay` restacks the descendant subtree.
- **`git stack amend`** rewrites the branch's *existing* tip commit with your
  staged changes (like `commit --amend`, but it also updates descendants).
  Engine: `git history fixup`. Use it to revise work already committed — e.g.
  addressing review feedback on a lower PR.

If you keep roughly one commit per branch/PR (a clean stacked-diffs style),
`amend` is your everyday tool; `commit` starts the next PR up.

## Conflict behaviour — important

The two engines handle conflicts differently, by design:

- **`amend` / `reword`** (`git history`) are **atomic**: if propagating would
  conflict with a descendant, the command **aborts and changes nothing**, with a
  message. Resolve the descendant, or use `commit` to add a separate commit.
- **`commit` / `restack` / `sync`** (`git replay`) fall back, on conflict, to a
  rebase that **persists the conflict markers into the committed files** so the
  operation always finishes. It then prints a **loud warning** and flags the
  affected branches. `git stack status` shows them with `⚠ conflict markers`.

**When you see that warning:** go to each flagged branch, search for
`<<<<<<<`, resolve the markers, and `git stack amend` (or `git add` +
`git commit --amend`) to clean it up. Never `submit` a branch that still has
markers.

## Safety notes

- `git stack sync` pushes with `--force-with-lease`, so it won't clobber commits
  a teammate pushed after your last fetch. If a push is rejected, run
  `git stack sync` again to pull their work in first.
- Use `git stack sync --no-push` to restack locally without touching the remote.
- git-stack never edits the trunk's commits; it only rebases your stack onto it.

## Worked example

```sh
git stack init                       # record trunk (auto-detects main/master)

git checkout main
git stack create types               # bottom of the stack
#   …edit, then:
git stack commit -m "Add domain types"
git stack describe -m "Introduces the core domain types shared by the stack."

git stack create service
git stack commit -m "Add service layer"

git stack create api
git stack commit -m "Expose HTTP API"

git stack status                     # see the three-branch stack
git stack submit                     # opens PRs #1/3, #2/3, #3/3, cross-linked

# Reviewer asks for a change on the bottom PR:
git stack down                       # → down to `types` (or: git checkout types)
#   …edit…
git add -A && git stack amend        # revise the commit; service+api auto-update
git stack submit                     # refresh all three PRs

# Trunk advanced / teammate pushed:
git stack sync                       # pull + restack + push (with lease)
```

Splitting an existing branch:

```sh
git checkout big-feature
git stack split                      # editor: prefix each commit with a branch
#   e.g.  api  <sha> Add API   /  api <sha> Add client  /  ui <sha> Add UI
git stack submit
```

## Pitfalls

- **Don't `git rebase` a tracked branch by hand.** Use `commit`/`amend`/`sync`.
- **`amend` needs staged changes** — `git add` first; it folds the *index* into
  the commit.
- **`amend` aborting on conflict is not a failure** — it's the safe outcome.
  Resolve the descendant or use `commit` instead.
- **`split` requires a clean work tree** and contiguous groups (all of one
  branch's commits together, in order). It doesn't reorder commits yet.
- **`git stack <cmd> --help` opens a man page** (git intercepts `--help`); use
  `git stack help` for the CLI help.
- After a conflicted `commit`/`sync`, **check `git stack status` for
  `⚠ conflict markers`** before submitting.

## Under the hood (for debugging)

- State: `git config --get-regexp '^branch\..*\.stack'` and `stack.trunk`.
- Restack engine: `git replay --onto <tip> --contained <old>..<leaf>` applied via
  `git update-ref --stdin` (fork-safe, no worktree); marker-persisting rebase
  fallback on conflict.
- Amend/reword engine: `git history fixup|reword` (atomic, updates all
  descendant branches; requires git ≥ 2.38 for the underlying machinery).
