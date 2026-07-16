#!/usr/bin/env bash
#
# Install the git-queue binaries (via `cargo install`) and the man page.
#
# Two binaries are installed: `git-queue` and its alias `git-q`, so both
# `git queue …` and `git q …` work. `cargo install` only ever copies binaries,
# so the man page is placed here separately. Once installed, `man git-queue`
# works, and so do `git queue --help` and `git q --help` (git routes those to
# the man pages).
set -euo pipefail

cd "$(dirname "$0")"

echo "==> cargo install --path ."
cargo install --path . --force

# Pick a man1 directory that is on the MANPATH and writable without sudo,
# preferring a system-wide location, then falling back to the user's home.
choose_man_dir() {
    for base in /usr/local/share/man "$HOME/.local/share/man"; do
        dir="$base/man1"
        if mkdir -p "$dir" 2>/dev/null && [ -w "$dir" ]; then
            printf '%s\n' "$dir"
            return 0
        fi
    done
    return 1
}

if man_dir="$(choose_man_dir)"; then
    echo "==> Generating man page into $man_dir"
    # Use the freshly installed binary to render the page from its own CLI.
    "$(command -v git-queue)" man --dir "$man_dir"
    # Same page under the alias name, so `git q --help` resolves too.
    cp "$man_dir/git-queue.1" "$man_dir/git-q.1"

    case "$man_dir" in
        "$HOME/.local/share/man/man1")
            if ! manpath 2>/dev/null | tr ':' '\n' | grep -qx "$HOME/.local/share/man"; then
                echo "note: add \$HOME/.local/share/man to your MANPATH so \`man git-queue\` is found, e.g.:"
                echo "      export MANPATH=\"\$HOME/.local/share/man:\$(manpath)\""
            fi
            ;;
    esac
    echo "==> Done. Try:  man git-queue   (or)   git queue --help   (or)   git q status"
else
    echo "warning: no writable man directory found; skipping man page." >&2
    echo "         Binary is installed; \`git queue help\` still works." >&2
fi
