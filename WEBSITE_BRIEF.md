# Design Brief — git-queue website

**For:** Claude Design
**From:** git-queue maintainers
**Deliverable:** A documentation-forward marketing site for `git-queue`, an open-source Git subcommand.
**Status of source material:** All product facts, commands, and examples in this brief are drawn from the shipping `README.md` and the Claude skill. Treat them as authoritative — do not invent commands, flags, or behaviours beyond what's here.

---

## 1. What git-queue is (the one-liner and the elevator pitch)

**One-liner:** Manage *stacks* of dependent branches and their *numbered pull requests*.

**Elevator pitch:** A *stack* is an ordered series of branches where branch *N* is built on top of branch *N-1*. That gives the stack a well-defined merge order — the bottom branch merges into trunk first, then the next, and so on. `git-queue` tracks that order, keeps the whole stack rebased on trunk, and opens **one PR per branch** — each targeting the branch below it, titled `[k/n]`, and cross-linked with a shared stack map. It's a single Rust binary named `git-queue`, so Git dispatches `git queue …` to it automatically.

**The problem it solves:** Big feature branches produce big, hard-to-review PRs. Splitting work into dependent branches is the fix, but doing it by hand means constant manual rebasing, broken PR bases, and reviewers merging things out of order. git-queue automates all of that.

**Who it's for:** Developers and teams who practice stacked-diffs / stacked-PR workflows on GitHub. Comfortable on the command line, care about small reviewable PRs, use `gh`.

---

## 2. Positioning & voice

- **Tone:** Precise, confident, engineer-to-engineer. No hype, no marketing fluff. The kind of copy that respects the reader's time. The existing README voice is the reference — keep it.
- **Personality:** This is a *sharp tool for people who already know Git*. Assume competence. Explain the model clearly, then get out of the way.
- **What to avoid:** "Revolutionary," "effortless," "10x," emoji-spam in body copy. (Emoji *do* appear meaningfully in the product — PR status glyphs like ✅/♻️/⏳/🟣/⚫ and the stack-map markers — so use them where they're functional, not decorative.)

---

## 3. Visual direction

The product's signature visual is the **ASCII stack diagram**. This should be the hero motif of the whole site:

```
◉ ui        #12 [OPEN]   ← current
◯ service   #11 [OPEN]
◯ api       #10 [OPEN]
┴
  main (trunk)
```

- **Aesthetic:** Terminal-native. Monospace as a first-class typeface, not just for code blocks. Think "beautifully typeset man page" rather than "SaaS landing page."
- **Palette suggestion:** Dark-terminal default with a crisp light mode. Use the red ✗ / green ✓ merge-order signalling as an accent system (it's literally how the product communicates). Filled vs. hollow nodes (◉ / ◯) distinguish current branch from the rest.
- **Motion (optional, tasteful):** The one place animation earns its keep is showing a stack being built branch-by-branch, or a PR landing and the stack re-numbering. Keep it subtle and skippable.
- **Must be responsive and theme-aware** (light/dark). Wide code blocks and tables scroll inside their own container — the page body never scrolls horizontally.

---

## 4. Sitemap / information architecture

Single-page-with-anchors *or* a few pages — designer's call — but these five sections are required and should be reachable from a persistent nav:

1. **Install**
2. **Getting started** (the core workflow)
3. **Command reference**
4. **Realistic examples**
5. **FAQ**

A short hero above all of it (pitch + the ASCII diagram + install command + a "Star on GitHub" / repo link). Footer links to the GitHub repo, `DESIGN.md`, and the Claude skill.

---

## 5. Page-by-page content

### 5.1 Install

Lead with the full path (binary + man page):

```sh
git clone git@github.com:freshtonic/git-queue.git
cd git-queue
./install.sh          # cargo install (the binary) + install the man page
git queue --version
```

Explain: `install.sh` runs `cargo install --path .` then writes the man page onto your `MANPATH` (because `cargo install` only installs *binaries*). Afterwards both `man git-queue` and `git queue --help` work — Git routes `git queue --help` to `man git-queue`; use `git queue help` for inline CLI help.

Show the binary-only path too:

```sh
cargo install --path .
```

Note the dispatch mechanic: because the binary is named `git-queue`, Git runs `git queue …` for you (standard subcommand mechanism).

**Optional: auto-restack hooks** (present as a callout, per-repo):

```sh
git queue hooks install     # writes post-commit / post-rewrite hooks
git queue hooks uninstall   # remove them
```

Without hooks, use `git queue commit` / `git queue amend` explicitly.

**Requirements** (render as a tidy list):
- **git** — `commit`/`sync` use `git replay` (git ≥ 2.44); `amend`/`reword` use `git history` (git ≥ 2.55). Other commands work on older git and will tell you if a feature is missing.
- **[`gh`](https://cli.github.com)** — authenticated GitHub CLI, for `submit`/`yank`. Not needed for local-only stack operations.

Small footnote: the man page is generated from the CLI definition (`clap_mangen`) so it never drifts; regenerate with `git-queue man --dir <man1-dir>`.

### 5.2 Getting started

This is the money section. Present the core workflow as an annotated, copyable sequence, ideally with the stack diagram evolving alongside it:

```sh
git queue init                 # record the trunk (auto-detects main/master)

git checkout main
git queue create api           # new branch on trunk; make commits
git queue create service       # new branch on top of `api`; make commits
git queue create ui            # new branch on top of `service`; make commits

git queue status               # view the stack and PR states
git queue down / up            # walk down/up the stack (aliases: prev/next)

git queue submit               # push all branches + open/update numbered PRs
# ... trunk moves on, a teammate pushes, or you amend a lower branch ...
git queue sync                 # pull remote, restack onto trunk, push (with lease)
git queue submit               # refresh the PRs
```

Then teach the **one distinction that matters most** — `commit` vs `amend` — as a highlighted pair:
- **`git queue commit`** adds a *new* commit (new work), then restacks all descendants. Engine: `git replay`.
- **`git queue amend`** folds *staged* changes into the branch's existing tip commit and updates descendants — like `commit --amend` but stack-aware. Engine: `git history fixup`. This is the everyday tool for addressing review feedback on a lower PR.

Include **the golden rule** as a prominent callout: *Don't hand-rebase a stack.* When you change a branch that has descendants, use the git-queue command for it so the change propagates to every branch above (forks included). `git rebase` on a tracked branch detaches its children.

### 5.3 Command reference

Render as a searchable/filterable table. Full command set:

| Command | Description |
|---|---|
| `git queue init [--trunk <b>]` | Record the trunk branch for this repo. |
| `git queue create <name>` | Create `<name>` on top of the current branch and track it. |
| `git queue split` | Split the current branch's commits into a stack (editor assigns commits to branches). |
| `git queue track [--parent <b>]` | Adopt the current branch into a stack (parent defaults to trunk). |
| `git queue untrack` | Forget the current branch's stack metadata. |
| `git queue describe [-m <text>]` | Describe the branch/PR; becomes the PR body (opens `$EDITOR` without `-m`). |
| `git queue status` (`ls`, `list`) | Show the stack tree with PR numbers/states. |
| `git queue up` / `down` (`next`/`prev`) | Check out the child / parent branch. |
| `git queue commit [-m <msg>]` | New commit on the current branch, then restack all descendants onto the new tip. |
| `git queue amend` | Fold **staged** changes into the current commit and update every descendant. |
| `git queue reword [<commit>]` | Rewrite a commit message and update descendants (defaults to HEAD). |
| `git queue restack` | Restack the current branch's descendants onto its tip. |
| `git queue hooks install` / `uninstall` | Make plain `git commit`/amend auto-restack descendants. |
| `git queue sync [--no-push]` | Pull remote, drop merged-PR branches (reparenting children), restack onto trunk, push with `--force-with-lease`. |
| `git queue submit [--draft]` (`push`) | Push the current stack line and open/update its numbered PRs (revives a child PR GitHub closed when its base was deleted). |
| `git queue yank` | Close every open (non-merged) PR in the current stack. |
| `git queue protect` | Enable merge-order signalling (a red/green commit status per PR) for this repo. |
| `git queue doctor` | Report whether merge-order signalling is enabled (read-only). |

Consider grouping visually into: **Setup** (init, create, track, untrack, split), **Navigate/inspect** (status, up, down), **Edit** (commit, amend, reword, restack, hooks), **Remote/PRs** (sync, submit, yank, describe), **Merge-order** (protect, doctor).

Note the help quirk near this table: `git queue <cmd> --help` is intercepted by Git and opens the man page; use `git queue help` for inline CLI help.

### 5.4 Realistic examples

Give three worked, end-to-end scenarios. These are the pages that sell the tool — make them feel like real sessions.

**Example A — Build and ship a three-PR stack (types → service → api).**
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
```

**Example B — Address review feedback on the bottom PR.** (Shows why `amend` is the everyday tool.)
```sh
git queue down                       # → down to `types`
#   …edit…
git add -A && git queue amend        # revise the commit; service+api auto-update
git queue submit                     # refresh all three PRs

# Trunk advanced / teammate pushed:
git queue sync                       # pull + restack + push (with lease)
```

**Example C — Split one big branch into a reviewable stack.**
```sh
git checkout big-feature
git queue split                      # editor: prefix each commit with a branch name
#   e.g.  api  <sha> Add API  /  api <sha> Add client  /  ui <sha> Add UI
git queue submit
```

For each example, pair the commands with the resulting **PR-body stack map** git-queue generates, so readers see the payoff. Use this real rendering:

```markdown
### 📚 Stacked PR · 2 of 3

Part of a stack. The PRs merge in FIFO order — the numbered order below, #1 first.

1. 🟣 [#10 `api`](…/pull/10) → `main`
2. ♻️🟢 **[#11 `service`](…/pull/11) → `api`**  👈 **this PR**
3. ⏳🟢 [#12 `ui`](…/pull/12) → `service`

---

<your `git queue describe` text>
```

### 5.5 FAQ

Author these from the source material. Suggested questions and answers:

- **In what order do the PRs merge?** FIFO — the oldest PR (`#1`, base of the stack) merges first, then the next. After a PR lands, run `git queue sync` (detects the merge, reparents branches above onto trunk, rebases them), then `git queue submit` (retargets PR bases, refreshes the list).

- **Can git-queue stop people merging out of order?** Yes — `git queue protect` (once per repo) enables a `git-queue/merge-order` commit status on every open PR: green ✓ on the bottom mergeable PR, red ✗ ("merge PR #N first") on every PR above it, with a link to the PR that must land first. It's **advisory** — the red ✗ shows in the checks list but doesn't disable the merge button; PRs stay normal, reviewable, non-draft. `git queue doctor` reports whether it's on.

- **Why a commit status instead of actually blocking the merge?** Every hard block GitHub offers lives on the *base* branch's rules. With base-chaining, a non-bottom PR targets an intermediate stack branch — so a ruleset that gates the merge also rejects git-queue's own pushes to that branch. Draft PRs (the one PR-level block) read socially as "not ready for review." A commit status is the strongest signal that keeps PRs normal and pushes unblocked.

- **What happens when a commit in the middle of the stack conflicts?** Depends on the operation. `commit`/`restack`/`sync` (engine `git replay`) always finish — on conflict they **persist the conflict markers into the committed files** and flag the affected branches (shown as `⚠ conflict markers` in `git queue status`). `amend`/`reword` (engine `git history`) are **atomic** — on conflict they **abort and change nothing**. When you see markers: go to each flagged branch, search for `<<<<<<<`, resolve, and `git queue amend`. Never `submit` a branch that still has markers.

- **What if the merged branch gets deleted?** Handled. If a merge deletes the branch (`--delete-branch` or repo auto-delete), `sync` reparents orphaned children onto trunk, and because GitHub closes a PR whose base was deleted, `submit` revives that child PR (reopening it or opening a fresh one retargeted to trunk).

- **Is my work safe from clobbering teammates?** `git queue sync` pushes with `--force-with-lease`, so it won't overwrite commits pushed after your last fetch. If a push is rejected, run `sync` again to pull their work in first. Use `git queue sync --no-push` to restack locally without touching the remote. git-queue never edits trunk's commits.

- **Where does git-queue store its state?** Entirely in the repo's own git config — nothing outside git. Keys: `queue.trunk`, `queue.remote`, `branch.<n>.queueParent`, `branch.<n>.queueParentSha`, `branch.<n>.queuePr`, `branch.<n>.queueDescription`, `queue.gate`. Inspect with `git config --get-regexp '^branch\..*\.queue'`. (Conflict-marker state is *not* stored — `status` detects `<<<<<<<` live, so it's never stale.)

- **Do I have to use `git queue commit` instead of `git commit`?** No — run `git queue hooks install` and a post-commit / post-rewrite hook calls `git queue restack` for you after plain `git commit` / `git commit --amend`. The hooks are guarded against recursion and no-op off a stack.

- **Why does `git queue --help` open a man page?** Git intercepts `--help` on subcommands and routes it to `man git-queue`. Install via `./install.sh` so that page exists; otherwise use `git queue help` or `git-queue --help` for inline help.

---

## 6. Cross-cutting content to weave in

- A short **"How it works"** explainer (can be its own section or an expandable panel): branches form a forest under trunk via parent pointers; a *stack line* is the linear chain from trunk to a leaf. `submit` pushes bottom-first (so each PR's base exists), points each PR at the branch below it, writes `[k/n]` titles, and prepends the linked stack map to every PR body (idempotently, bounded by hidden markers). Link out to `DESIGN.md` for the full design.
- A callout that there's a **Claude Code skill** (`skills/using-git-queue/SKILL.md`, CipherPowers format) teaching Claude to drive `git queue` — copyable into a plugin's `skills/` directory.

## 7. Success criteria

- A developer who's never seen git-queue can install it and ship their first stacked PR set using only this site.
- Every command, flag, and behaviour on the site matches the source exactly — no invented capabilities.
- The site *feels* like the tool: terminal-native, precise, respectful of the reader's expertise.
- Fully responsive, light/dark theme-aware, and readable as static content (no build step required to consume it).
