#!/usr/bin/env bash
#
# Install the git-stack binary (via `cargo install`) and its man page.
#
# `cargo install` only ever copies binaries, so the man page is placed here
# separately. Once installed, `man git-stack` works, and so does
# `git stack --help` (git routes that to `man git-stack`).
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
    "$(command -v git-stack)" man --dir "$man_dir"

    case "$man_dir" in
        "$HOME/.local/share/man/man1")
            if ! manpath 2>/dev/null | tr ':' '\n' | grep -qx "$HOME/.local/share/man"; then
                echo "note: add \$HOME/.local/share/man to your MANPATH so \`man git-stack\` is found, e.g.:"
                echo "      export MANPATH=\"\$HOME/.local/share/man:\$(manpath)\""
            fi
            ;;
    esac
    echo "==> Done. Try:  man git-stack   (or)   git stack --help"
else
    echo "warning: no writable man directory found; skipping man page." >&2
    echo "         Binary is installed; \`git stack help\` still works." >&2
fi
