# `git queue tui` mutates git state immediately, backed by an undo stack

## Context

The interactive editor could either **batch** edits into a rebase-todo applied on
exit (like `git rebase -i`), or **mutate** real git state after each operation.
We chose **immediate mutation**: every reorder, squash, split, reword, and delete
performs its rebase the moment it is made, so the panes always show true,
materialised history — including real conflicts and real new SHAs. It is the only
model consistent with an editable, live diff/message view and with reporting a
conflict *at the moment a move causes it*.

## Considered Options

- **Immediate mutation** (chosen) — truthful live feedback; conflicts surface as
  they happen. Cost: they happen *mid-session*, and undo must un-rewrite real
  commits.
- **Batched rebase-todo** — safer and trivially undoable (nothing applied until
  the end), but cannot truthfully show a reorder's conflict or a live diff before
  applying. This is `rebase -i`'s known weakness.

## Consequences

Immediate mutation obligates two companion subsystems, both non-negotiable:

- **Undo/redo.** Before each operation, snapshot all queue-line refs and queue
  metadata; undo restores the snapshot (rewritten commit *objects* survive in git
  until GC). Unlimited within a session, session-scoped, with **git reflog as the
  durable cross-session backstop**. A quit-guard confirms exit while operations
  are on the stack.
- **Conflict escape hatch.** An operation that conflicts prompts *resolve or
  undo*. *Resolve* **suspends to the shell** — the user is left in the standard
  mid-rebase state, resolves with their normal git workflow, and re-enters to
  resume; the TUI owns no conflict-resolution UI. *Undo* backs the operation out
  cleanly.
- Because both exist, no predictive pre-conflict check is needed: a conflicting
  move is simply attempted, and a bad one is one undo away.
