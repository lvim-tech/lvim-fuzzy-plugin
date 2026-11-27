#!/bin/sh
# Install what the lvim tools and the Neovim plugins need at run time — from nothing but git and cargo. Every part is fetched (a checkout
# next to this one is used when there is one: the developer's), built and put in place. Nothing needs to be installed first, in either
# direction: the terminal programs work without the Neovim plugins, and the Neovim plugins without the terminal programs.
#
#   sh bootstrap.sh [part ...]
#
# parts:  host            lvim-plugin, the plugin host (git, remote, devices, preview, open are served through it)
#         git remote devices preview open     the plugins (each: its program in ~/.local/bin, its plugin.toml in ~/.config/lvim-plugins/)
#         cli             lvim-fuzzy-cli (lfz): the Neovim picker's modes come from it
#         commander-host  lvim-commander-host: the core the Neovim commander runs
#         commander       lvim-fuzzy-commander (lvc)
#   default: host git remote devices preview open
#
# PREFIX (default ~/.local) is where programs go; LVIM_SRC (default ~/.local/share/lvim-tech/src) is where the sources are cloned to.
set -e
# (piped into sh — `curl … | sh -s -- parts` — there is no script file: everything is cloned)
if [ -f "$0" ]; then here=$(cd "$(dirname "$0")/.." && pwd); else here=/nonexistent; fi
src=${LVIM_SRC:-${XDG_DATA_HOME:-$HOME/.local/share}/lvim-tech/src}
prefix=${PREFIX:-$HOME/.local}
bin=$prefix/bin
mkdir -p "$bin"

command -v git >/dev/null 2>&1 || { echo "bootstrap: git is needed" >&2; exit 1; }
command -v cargo >/dev/null 2>&1 || { echo "bootstrap: cargo is needed (https://rustup.rs)" >&2; exit 1; }

# the folder with the sources of repository $1: this one, a checkout next to it, else a clone (kept up to date)
checkout() {
    if [ "$(basename "$here")" = "$1" ]; then echo "$here"; return; fi
    if [ -d "$(dirname "$here")/$1/.git" ]; then echo "$(dirname "$here")/$1"; return; fi
    mkdir -p "$src"
    if [ -d "$src/$1/.git" ]; then
        git -C "$src/$1" pull -q --ff-only >&2 || true
    else
        git clone -q --depth 1 "https://github.com/lvim-tech/$1.git" "$src/$1" >&2
    fi
    echo "$src/$1"
}

# build repository $1 and install its program $2 (as $3, default the same)
program() {
    d=$(checkout "$1")
    echo "== $1"
    (cd "$d" && cargo build --release -q)
    install -m 755 "${CARGO_TARGET_DIR:-$d/target}/release/$2" "$bin/${3:-$2}.new"
    mv "$bin/${3:-$2}.new" "$bin/${3:-$2}"
    echo "installed: $bin/${3:-$2}"
}

[ $# -gt 0 ] || set -- host git remote devices preview open
for part in "$@"; do
    case "$part" in
        host) program lvim-fuzzy-plugin lvim-fuzzy-plugin lvim-plugin ;;
        git | remote | devices | preview | open)
            d=$(checkout "lvim-fuzzy-plugin-$part")
            echo "== lvim-fuzzy-plugin-$part"
            XDG_BIN_HOME="$bin" sh "$d/scripts/install.sh"
            ;;
        cli) program lvim-fuzzy-cli lvim-fuzzy-cli ;;
        commander-host) program lvim-fuzzy-commander-core lvim-commander-host ;;
        commander)
            d=$(checkout lvim-fuzzy-commander)
            echo "== lvim-fuzzy-commander"
            sh "$d/scripts/install.sh"
            ;;
        *) echo "bootstrap: unknown part '$part'" >&2; exit 1 ;;
    esac
done
case ":$PATH:" in *":$bin:"*) ;; *) echo "note: $bin is not in PATH" ;; esac
