---
name: Using git-queue
description: Manage stacks of dependent branches and their numbered pull requests with the `git queue` subcommand
when_to_use: when work spans several dependent branches that should become separate, reviewable PRs; when editing a branch that has other branches stacked on top of it; when keeping a stack rebased on trunk and its PRs in sync; when a large branch should be split into a reviewable stack; when PRs must not be merged out of order
version: 1.1.0
---

# Using git-queue

## Overview

`git queue` manages **stacks**: ordered series of branches where branch *N* is
built on top of branch *N-1*. The stack has a merge order — the bottom branch
merges into trunk first, then the next, and so on. git-queue tracks that order,
keeps the whole stack rebased, and opens one **numbered pull request per
branch**, each targeting the branch below it so every PR shows only its own
diff.

Prefer a stack over one giant branch whenever a change has separable parts
(types → service → API → UI): reviewers get small PRs, and each lands
independently in order.

**Mental model.** Each tracked branch stores its parent in git config
(`branch.<n>.queueParent`). Branches form a forest under `trunk`; a *stack line*
is the linear chain from trunk to a leaf. Nothing lives outside git — inspect
state with `git config --get-regexp '^branch\..*\.queue'`.

## The golden rule: don't hand-rebase a stack

When you change a branch that has descendants, **use the git-queue command for
it** so the change propagates to every branch above (forks included). Do not
`git rebase` stack branches by hand — you'll detach the children. The commands
below own that propagation.

## Choosing the right command

| Situation | Command | What happens |
|---|---|---|
| Start a new branch on top of the current one | `git queue create <name>` | Creates + tracks it |
| Add **new** work to a branch that has descendants | `git queue commit [-m …]` | New commit, then restacks all descendants |
| **Change an existing commit** on a mid-stack branch | `git add …` then `git queue amend` | Folds staged changes in, updates descendants |
| Fix a commit **message** | `git queue reword [<commit>]` | Rewrites message, updates descendants |
| Trunk moved / a teammate pushed / branches drifted | `git queue sync` | Pulls remote commits, restacks on trunk, pushes back |
| Open or refresh the PRs | `git queue submit` | Pushes + numbered, cross-linked PRs |
| Say what a PR is about | `git queue describe [-m …]` | Sets the PR body |
| One big branch → a stack | `git queue split` | Editor assigns commits to branches |
| Abandon a stack's open PRs | `git queue yank` | Closes every open (non-merged) PR in the stack |
| Stop PRs merging out of order | `git queue protect` | Enables merge-order statuses on each PR (one-time) |
| Check enforcement is on | `git queue doctor` | Read-only report of the gate status |
| Move around the stack | `git queue up` / `down`, `git queue status` | Navigate / view |

### commit vs amend — the key distinction

- **`git queue commit`** adds a *new* commit. Use it for genuinely new work.
  Engine: `git replay` restacks the descendant subtree.
- **`git queue amend`** rewrites the branch's *existing* tip commit with your
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
  affected branches. `git queue status` shows them with `⚠ conflict markers`.

**When you see that warning:** go to each flagged branch, search for
`<<<<<<<`, resolve the markers, and `git queue amend` (or `git add` +
`git commit --amend`) to clean it up. Never `submit` a branch that still has
markers.

## Enforcing merge order

To warn reviewers off merging PRs out of order, run `git queue protect` **once**
per repo. It enables *status-based* gating (`queue.gate = status`) — no GitHub
workflow, ruleset, or admin rights needed. Then `git queue submit` posts a
`git-queue/merge-order` commit status on every open PR: green ✓ on the
**bottom** (mergeable) PR, red ✗ ("merge PR #N first", linking to that PR) on
every PR above it. As the bottom PR lands, `git queue sync` + `git queue submit`
promote the next one to green.

- `git queue doctor` reports whether the gate is enabled (read-only).
- The gate is **advisory**: the red ✗ shows in the checks list but does not
  disable the merge button. PRs stay normal, reviewable, non-draft PRs.
- A status — not a draft, label, or ruleset — because every hard merge block
  lives on the *base* branch's rules, and with base-chaining a non-bottom PR
  targets an intermediate branch, so a ruleset that gates the merge also blocks
  git-queue's pushes; drafts block the button but read as "not ready for
  review".
- Agents/scripts: check before merging — the status appears in
  `gh pr view <n> --json statusCheckRollup`; do not merge a stack PR whose
  `git-queue/merge-order` status is FAILURE.

## Safety notes

- `git queue sync` pushes with `--force-with-lease`, so it won't clobber commits
  a teammate pushed after your last fetch. If a push is rejected, run
  `git queue sync` again to pull their work in first.
- Use `git queue sync --no-push` to restack locally without touching the remote.
- git-queue never edits the trunk's commits; it only rebases your stack onto it.
- **Merging tip:** deleting the merged branch is fine — `git queue sync` reparents
  orphaned children onto trunk and `git queue submit` revives any PR GitHub closed
  because its base branch was deleted. Run `sync` then `submit` after each merge.

## Worked example

```sh
git queue init                       # record trunk (auto-detects main/master)

git checkout main
git queue create types               # bottom of the stack
#   …edit, then:
git queue commit -m "Add domain types"
git queue describe -m "Introduces the core domain types shared by the stack."

git queue create service
git queue commit -m "Add service layer"

git queue create api
git queue commit -m "Expose HTTP API"

git queue status                     # see the three-branch stack
git queue submit                     # opens PRs #1/3, #2/3, #3/3, cross-linked

# Reviewer asks for a change on the bottom PR:
git queue down                       # → down to `types` (or: git checkout types)
#   …edit…
git add -A && git queue amend        # revise the commit; service+api auto-update
git queue submit                     # refresh all three PRs

# Trunk advanced / teammate pushed:
git queue sync                       # pull + restack + push (with lease)
```

Splitting an existing branch:

```sh
git checkout big-feature
git queue split                      # editor: prefix each commit with a branch
#   e.g.  api  <sha> Add API   /  api <sha> Add client  /  ui <sha> Add UI
git queue submit
```

## Pitfalls

- **Don't `git rebase` a tracked branch by hand.** Use `commit`/`amend`/`sync`.
- **`amend` needs staged changes** — `git add` first; it folds the *index* into
  the commit.
- **`amend` aborting on conflict is not a failure** — it's the safe outcome.
  Resolve the descendant or use `commit` instead.
- **`split` requires a clean work tree** and contiguous groups (all of one
  branch's commits together, in order). It doesn't reorder commits yet.
- **`git queue <cmd> --help` opens a man page** (git intercepts `--help`); use
  `git queue help` for the CLI help.
- After a conflicted `commit`/`sync`, **check `git queue status` for
  `⚠ conflict markers`** before submitting.

## Under the hood (for debugging)

- State: `git config --get-regexp '^branch\..*\.queue'` and `queue.trunk`.
- Restack engine: `git replay --onto <tip> --contained <old>..<leaf>` applied via
  `git update-ref --stdin` (fork-safe, no worktree); marker-persisting rebase
  fallback on conflict.
- Amend/reword engine: `git history fixup|reword` (atomic, updates all
  descendant branches; requires git ≥ 2.38 for the underlying machinery).
