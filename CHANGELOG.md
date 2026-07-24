# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

From the next release onward this file is maintained automatically by
[release-plz](https://release-plz.dev) from the Conventional-Commit history.

## [0.1.1](https://github.com/freshtonic/git-queue/compare/v0.1.0...v0.1.1) - 2026-07-24

### Added

- Windows portability — cross-platform RNG, HOME lookup, browser open ([#11](https://github.com/freshtonic/git-queue/pull/11))

### Other

- adopt strict clippy/rust lints and clean the tree to pass them
- cargo fmt
- complete dist setup (v0.32) — shell installer + release workflow
- hand binary releases to dist (curl|sh installers)
- seed CHANGELOG and ship prebuilt binaries on each GitHub release

## [0.1.0] - 2026-07-24

Initial release.

- `git queue` — manage queues of dependent branches and their numbered,
  cross-linked pull requests: `create`, `status`, `log`, `edit`, `move`,
  `reword`, `amend`, `commit`, `requeue`, `sync`, `submit`, `track`, and more.
- `git queue tui` — an interactive terminal editor for the current queue line:
  reorder, reword, squash, split, and delete commits, and edit branch
  boundaries, with unlimited undo/redo and a resolve-or-undo conflict escape
  hatch.
- `git queue setup` — opt-in integrations: git hooks, the merge-order status
  gate, the man page, shell completion, a `git q` alias, and agent skills.
