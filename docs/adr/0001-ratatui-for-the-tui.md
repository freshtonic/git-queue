# Build the `git queue tui` interface on ratatui + crossterm

## Context

git-queue deliberately keeps a *tiny* dependency surface — it shells out to
`git` and `gh` rather than linking libgit2 (see `DESIGN.md`) — so any large
dependency is a departure from a stated value. The interactive queue/history
editor (`git queue tui`) is inherently in-process: unlike git operations, a TUI
cannot be "shelled out to." We chose **ratatui + crossterm**, the de-facto Rust
TUI stack (immediate-mode rendering, cross-platform raw mode, mouse capture,
active maintenance), accepting that this is by far the largest dependency the
project has taken.

## Considered Options

- **ratatui + crossterm** (chosen) — standard, well-documented, fits the custom
  three-pane layout and needs crossterm's mouse capture anyway.
- **cursive** — higher-level retained-mode framework; heavier and a poor fit for
  the custom-drawn split-pane layout.
- **Raw crossterm, no framework** — honours the austerity most, but means
  hand-rolling layout, scrolling, focus, and redraw for a genuinely complex app —
  effectively reinventing ratatui.

## Consequences

- The `git queue tui` view is written against ratatui's API; swapping frameworks
  later would be a real rewrite of the view layer (hence this ADR).
- To bound the spend, the **diff pane still shells out to `git`** and we colour
  `+`/`-`/hunk lines ourselves — **no syntax-highlighting dependency** (e.g.
  `syntect`) in the first version. The new dependency footprint is essentially
  just the TUI stack.
- The domain logic lives in a headless engine behind the view, so the dependency
  is confined to a thin, essentially-untested shell (see the engine/view seam in
  the design).
