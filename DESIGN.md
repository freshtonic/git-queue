# git-queue design

## Goal

A `git queue` subcommand that manages **queues** — ordered series of branches
where branch *N* depends on branch *N-1* — and the creation of **numbered pull
requests** from those branches.

## Domain model: parent pointers

Every tracked branch stores a pointer to its parent branch. Branches thus form
a **forest rooted at trunk**. A **queue line** is the linear chain from trunk up
to a leaf. This is the model used by Graphite/`gt`; it is chosen over an
explicit "named ordered list of branches" because:

- The parent relationship *is* the dependency, expressed once per branch.
- Branching a queue (two children on one branch) falls out naturally.
- Merge order is implied: FIFO along a line, front (oldest) first.

Trade-off: a "queue" is not a first-class named object, so operations pick the
line running through the current branch. Forks are supported for tracking and
`status`, but `submit`/numbering operate on a single line and warn at a fork.

## State: git config only

No sidecar files, no external database — state lives in the repo's git config so
it is versioned per-clone and inspectable with plain `git config`:

| Key | Meaning |
|---|---|
| `queue.trunk` | Trunk branch. |
| `queue.remote` | Remote (default `origin`). |
| `branch.<n>.queueParent` | Parent branch. |
| `branch.<n>.queueParentSha` | Parent tip at last (re)base — the **rebase anchor**. |
| `branch.<n>.queuePr` | Cached PR number. |

## Shelling out to `git` and `gh`

The tool drives the `git` and `gh` executables rather than linking a library.
Rebase and conflict behavior is then exactly what the user gets by hand, output
(progress, conflict markers) is native, and the dependency surface is tiny. The
cost — parsing text/JSON output — is small and localized in `git.rs` / `gh.rs`.

## Key algorithms

### `sync` — queued requeue

1. `git fetch <remote>` (non-fatal: falls back to local trunk when offline).
2. New trunk tip = `<remote>/<trunk>` if present, else local `<trunk>`.
3. Visit tracked branches in **topological order** (parents before children).
4. For each branch, rebase only its own commits onto the current parent tip:
   `git rebase --onto <parent-tip> <stored-anchor> <branch>`, then update the
   stored anchor to the new parent tip.
5. A conflict stops the run with instructions; re-running resumes cleanly
   because already-requeueed branches are detected as up to date (anchor ==
   parent tip) and skipped.

Using the stored anchor (not `merge-base`) is what keeps a parent's commits from
being replayed onto the child.

### `submit` — numbered PRs

For queue line `main ← A ← B ← C`:

- **Base chaining:** PR(A)→`main`, PR(B)→`A`, PR(C)→`B`. Each PR shows only its
  own diff.
- **Push order:** front-first, so every PR's base branch already exists on the
  remote (force-with-lease, sets upstream).
- **Two passes:** pass 1 pushes and creates-or-finds each PR to learn its
  number; pass 2 writes the `[k/n]` title, correct base, and a shared navigation
  block (BEGIN/END-delimited so a user's own body text is preserved) linking
  every PR in the line and marking the current one.
- Refuses to open a PR for a branch with no commits beyond its base.

## Command surface (MVP)

`init`, `create`, `status` (`ls`/`list`), `track`, `untrack`, `up`/`next`,
`down`/`prev`, `sync`, `submit` (`push`).

## Module layout

| File | Responsibility |
|---|---|
| `main.rs` | Clap CLI definition and dispatch. |
| `git.rs` | Process wrappers over `git`. |
| `gh.rs` | Process wrappers over `gh` (PR list/create/edit). |
| `meta.rs` | Read/write queue metadata in git config. |
| `queue.rs` | Domain model: chains, forks, topological order. |
| `render.rs` | Pure rendering: status tree, PR titles, nav block (unit-tested). |
| `commands.rs` | One function per subcommand. |

## Testing strategy

- **Unit tests** (`render.rs`): title prefixing, body-block compose/replace —
  pure, no git.
- **Integration tests** (`tests/integration.rs`): run the built binary against
  throwaway repos (tempdirs) covering `init`, `create`, navigation, `track`,
  `untrack`, and the queued `sync`. `submit` is excluded because it needs an
  authenticated `gh` and a GitHub remote.

## Known limitations / future work

- `submit` operates on one linear line; forked queues warn and submit one line.
- `sync` conflict recovery is resume-by-re-run, not an explicit `--continue`.
- No `git queue merge` / land automation yet (rely on GitHub merge queue).
- Forge is GitHub via `gh` only; a `Forge` trait could add GitLab later.
