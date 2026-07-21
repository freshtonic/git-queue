---
name: Using git-queue
description: Manage queues of dependent branches and their numbered pull requests with the `git queue` subcommand
when_to_use: when work spans several dependent branches that should become separate, reviewable PRs; when editing a branch that has other branches queued behind it; when keeping a queue rebased on its base branch and its PRs in sync; when a large branch should be split into a reviewable queue; when PRs must not be merged out of order
version: 1.1.0
---

# Using git-queue

## Overview

`git queue` manages **queues**: ordered series of branches where branch *N* is
built on branch *N-1*. The queue has a FIFO merge order — the front branch
merges into trunk first, then the next, and so on. git-queue tracks that order,
keeps the whole queue rebased, and opens one **numbered pull request per
branch**, each targeting the branch below it so every PR shows only its own
diff.

Prefer a queue over one giant branch whenever a change has separable parts
(types → service → API → UI): reviewers get small PRs, and each lands
independently in order.

**Mental model.** Each tracked branch stores its parent in git config
(`branch.<n>.queueParent`). Branches form a forest under `trunk`; a *queue line*
is the linear chain from trunk to a leaf. Nothing lives outside git — inspect
state with `git config --get-regexp '^branch\..*\.queue'`.

## The golden rule: don't hand-rebase a queue

When you change a branch that has descendants, **use the git-queue command for
it** so the change propagates to every branch above (forks included). Do not
`git rebase` queue branches by hand — you'll detach the children. The commands
below own that propagation.

## Choosing the right command

| Situation | Command | What happens |
|---|---|---|
| Queue a new branch after the current one | `git queue create <name>` | Creates + tracks it |
| Start a queue on another base (e.g. a release branch) | `git queue create <name> --base <branch>` | The front PR will target `<branch>` |
| Add **new** work to a branch that has descendants | `git queue commit [-m …]` | New commit, then requeues all descendants |
| **Change an existing commit** on a mid-queue branch | `git add …` then `git queue amend` | Folds staged changes in, updates descendants |
| Fix a commit **message** | `git queue reword [<commit>]` | Rewrites message, updates descendants |
| **Move** a commit (or inclusive range `<a>..<b>`) elsewhere in the queue | `git queue move <commit> --new-parent <commit>` | Works within a PR or across PRs; both commits must be in the queue; the rest is requeued, conflicts persist as markers |
| Trunk moved / a teammate pushed / branches drifted | `git queue sync` | Pulls remote commits, requeues on the base, pushes back, reconciles published PRs |
| Open or refresh the PRs | `git queue submit` | Pushes + numbered, cross-linked PRs |
| Turn an existing PR into a queue | `git queue track`, then `create` more, then `sync`/`submit` | The existing PR is adopted: title kept (numbered), body preserved under the queue map; missing PRs are opened |
| Name the queue (mandatory; prompted at creation) | `git queue name [<name>]` | Shown in PR headers; keys the queue description |
| Describe the whole queue | `git queue describe [-m …]` | "About this queue" section in every PR of the queue |
| Describe one branch | `git queue describe-branch [-m …]` | "About this branch" section in its PR |
| List all queues | `git queue ls` | Most recently touched first |
| One big branch → a queue | `git queue split` | Editor assigns commits to branches |
| Adopt an existing branch AND divide it | `git queue track --split` (add `--stamp-ids` to skip the prompt) | track + stamp + split editor in one step |
| Abandon a queue's open PRs | `git queue yank` | Closes every open (non-merged) PR in the queue |
| Stop PRs merging out of order | `git queue protect` | Enables merge-order statuses on each PR (one-time) |
| Check enforcement is on | `git queue doctor` | Read-only report of the gate status |
| Give commits stable identity across rewrites | `git queue hooks install`, `git queue commit`, or `git queue track --stamp-ids` for existing commits | Stamps a `Stable-Commit-Id:` trailer; powers safe sync (no self-conflicts) and squash-merge detection |
| Move around the queue | `git queue up` / `down`, `git queue status` | Navigate / view |
| See every commit in the queue with its Stable-Commit-Id | `git queue log` | Status tree + indented per-branch commits, newest first |
| Address a commit by its id | `git queue move q-3zz02424 --new-parent <rev>`, `git queue reword q-…` | Any commit argument accepts a `Stable-Commit-Id` (unique prefix ok), as shown by `git queue log` |
| Edit a commit in place, anywhere in the queue | `git queue checkout <commit-or-id>`, edit, `git add`, then `git commit` (insert) or `git commit --amend` (revise; id preserved), then `git queue requeue` if hooks aren't installed | The rest of the queue rebases on top; reattach with `git queue checkout <branch>` |

### commit vs amend — the key distinction

- **`git queue commit`** adds a *new* commit. Use it for genuinely new work.
  Engine: `git replay` requeues the descendant subtree.
- **`git queue amend`** rewrites the branch's *existing* tip commit with your
  staged changes (like `commit --amend`, but it also updates descendants).
  Engine: `git history fixup`. Use it to revise work already committed — e.g.
  addressing review feedback on a lower PR.

If you keep roughly one commit per branch/PR (a clean queued-diffs style),
`amend` is your everyday tool; `commit` starts the next PR up.

## Conflict behaviour — important

The two engines handle conflicts differently, by design:

- **`amend` / `reword`** (`git history`) are **atomic**: if propagating would
  conflict with a descendant, the command **aborts and changes nothing**, with a
  message. Resolve the descendant, or use `commit` to add a separate commit.
- **`commit` / `requeue` / `sync`** (`git replay`) fall back, on conflict, to a
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
**front** (mergeable) PR, red ✗ ("merge PR #N first", linking to that PR) on
every PR behind it. As the front PR lands, `git queue sync` + `git queue submit`
promote the next one to green.

- `git queue doctor` reports whether the gate is enabled (read-only).
- The gate is **advisory**: the red ✗ shows in the checks list but does not
  disable the merge button. PRs stay normal, reviewable, non-draft PRs.
- A status — not a draft, label, or ruleset — because every hard merge block
  lives on the *base* branch's rules, and with base-chaining a non-front PR
  targets an intermediate branch, so a ruleset that gates the merge also blocks
  git-queue's pushes; drafts block the button but read as "not ready for
  review".
- Agents/scripts: check before merging — the status appears in
  `gh pr view <n> --json statusCheckRollup`; do not merge a queue PR whose
  `git-queue/merge-order` status is FAILURE.

## Safety notes

- `git queue sync` pushes with `--force-with-lease`, so it won't clobber commits
  a teammate pushed after your last fetch. If a push is rejected, run
  `git queue sync` again to pull their work in first.
- Use `git queue sync --no-push` to requeue locally without touching the remote.
- git-queue never edits the trunk's commits; it only rebases your queue onto it.
- **Merging tip:** deleting the merged branch is fine — `git queue sync` reparents
  orphaned children onto the queue's base and `git queue submit` revives any PR GitHub closed
  because its base branch was deleted. Run `sync` then `submit` after each merge.

## Worked example

```sh
git queue init                       # record trunk (auto-detects main/master)

git checkout main
git queue create types               # front of the queue
#   …edit, then:
git queue commit -m "Add domain types"
git queue describe -m "Introduces the core domain types shared by the queue."

git queue create service
git queue commit -m "Add service layer"

git queue create api
git queue commit -m "Expose HTTP API"

git queue status                     # see the three-branch queue
git queue submit                     # opens PRs #1/3, #2/3, #3/3, cross-linked

# Reviewer asks for a change on the front PR:
git queue down                       # → down to `types` (or: git checkout types)
#   …edit…
git add -A && git queue amend        # revise the commit; service+api auto-update
git queue submit                     # refresh all three PRs

# Trunk advanced / teammate pushed:
git queue sync                       # pull + requeue + push (with lease)
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
- Requeue engine: `git replay --onto <tip> --contained <old>..<leaf>` applied via
  `git update-ref --stdin` (fork-safe, no worktree); marker-persisting rebase
  fallback on conflict.
- Amend/reword engine: `git history fixup|reword` (atomic, updates all
  descendant branches; requires git ≥ 2.38 for the underlying machinery).
