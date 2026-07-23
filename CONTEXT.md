# Context: git-queue glossary

The canonical vocabulary for git-queue. When code, docs, issues, or PRs name a
domain concept, use the term as defined here. This file is a glossary — not a
spec, not a scratchpad, not a home for implementation decisions.

## Change

A unit of work with a **stable identity that survives rewriting**. A change is
identified by its [Stable-Commit-Id](#stable-commit-id), which persists across
amend, rebase, cherry-pick, and replay even as the underlying
[commit](#commit) hash churns. "The same change" means "the same
Stable-Commit-Id", regardless of how many times its commit has been rewritten.

## Commit

A specific git commit object, named by its SHA. A commit is the *materialisation*
of a [change](#change) at a point in time; every amend, move, or requeue mints a
new commit (new SHA) for the same change. SHAs are worthless as long-lived
identifiers in this rewrite-heavy workflow — that is what the
[Stable-Commit-Id](#stable-commit-id) is for.

## Stable-Commit-Id

The stable identity of a [change](#change), carried as a `Stable-Commit-Id: q-…`
trailer in the commit message (the same idea as Gerrit's `Change-Id`). Because
the message travels faithfully through rebase and amend, the trailer gives each
change a fixed identity across rewrites. git-queue uses it to tell a teammate's
new work apart from a stale copy of one's own commits, and to detect
squash-merged work whose SHAs and patch-ids are gone.

## Queue line

The linear chain of [branches](#boundary) from [trunk](#front) up to a single
leaf, each branch pointing to its parent. Operations that need a single ordering
(`submit`, `tui`) act on the queue line running through the current branch.

## Boundary

The division between two adjacent branches within a [queue line](#queue-line). A
branch owns a contiguous run of commits; a boundary is where one branch's run
ends and the next begins. A **boundary edit** (rename / add / remove / shift a
branch) only moves refs — no [commit](#commit) is rewritten and no
[change](#change) identity is affected.

## Fork

A [branch](#boundary) with more than one tracked child — the point where the
[forest](#front) branches. `status` renders forks; single-line operations
(`submit`, `tui`) act on one line and treat a fork as a boundary they will not
cross.

## Front / Tip

**Front** is the oldest end of a [queue line](#queue-line) — nearest **trunk**
(the root of the branch forest), the end that merges first. **Tip** is the
newest end — the leaf branch's latest [commit](#commit). Front-first is the
canonical ordering: front at the top of a listing, tip at the bottom.

## Divergence (disallowed)

Two distinct [commits](#commit) sharing one [Stable-Commit-Id](#stable-commit-id).
Jujutsu permits divergence as a first-class state; git-queue does **not** — a
change identity is unique, because divergence would defeat the teammate-vs-stale
detection that ids exist for. Single-line editing cannot manufacture divergence,
so nothing needs to police it; the invariant is recorded here as a deliberate
departure from jj.
