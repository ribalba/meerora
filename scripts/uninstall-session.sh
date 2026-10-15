#!/usr/bin/env bash
# uninstall-session.sh removes what scripts/install-session.sh added, after a confirmation.
#
#   sudo scripts/uninstall-session.sh            remove the meerora session
#   scripts/uninstall-session.sh --dry-run       print what would go and change nothing
#
# It reads the installer's manifest in <prefix>/share/meerora and removes only
# the paths the installer writes. Your data in ~/.local/share/meerora stays.

set -euo pipefail

prefix=/usr/local
root=""
dry=0
yes=0

usage() {
    cat <<'EOF'
Usage: sudo scripts/uninstall-session.sh [options]

Options:
  --dry-run      Print what would be removed and change nothing. Needs no sudo.
  --yes          Answer yes to every question, which also removes cage when the installer installed it.
  --prefix DIR   The prefix given to the installer, /usr/local by default.
  --root DIR     The staging root given to the installer. Needs no sudo and removes no packages.
  -h, --help     Show this help.
EOF
}

while [ $# -gt 0 ]; do
    case "$1" in
        --dry-run) dry=1 ;;
        --yes) yes=1 ;;
        --prefix)
            prefix="${2:?--prefix needs a directory}"
            shift
            ;;
        --root)
            root="${2:?--root needs a directory}"
            shift
            ;;
        -h | --help)
            usage
            exit 0
            ;;
        *)
            printf 'Unknown option %s.\n\n' "$1" >&2
            usage >&2
            exit 2
            ;;
    esac
    shift
done
case "$prefix" in
    /*) prefix="${prefix%/}" ;;
    *)
        echo "--prefix needs an absolute path." >&2
        exit 2
        ;;
esac
if [ -n "$root" ]; then
    root="$(realpath -m "$root")"
fi

share_dir="$prefix/share/meerora"
manifest="$share_dir/install-manifest.txt"
# The only paths this script ever removes, the same list the installer writes.
KNOWN_FILES=("$prefix/bin/meerora" "$prefix/bin/meerora-agentd" "$prefix/bin/meerora-mcp" "$prefix/bin/meerora-session"
    /usr/share/wayland-sessions/meerora.desktop "$prefix/lib/systemd/user/meerora-session.target"
    "$prefix/share/xdg-desktop-portal/meerora-portals.conf" "$prefix/share/applications/meerora.desktop")
for s in 16 32 48 64 128 256 512; do
    KNOWN_FILES+=("$prefix/share/icons/hicolor/${s}x$s/apps/meerora.png")
done

say() {
    printf '%s\n' "$*"
}

fail() {
    printf 'uninstall-session.sh stopped. %s\n' "$*" >&2
    exit 1
}

quoted() {
    local out=() a
    for a in "$@"; do
        out+=("$(printf '%q' "$a")")
    done
    printf '%s' "${out[*]}"
}

run() {
    if [ "$dry" = 1 ]; then
        say "  (dry run) \$ $(quoted "$@")"
        return 0
    fi
    say "  \$ $(quoted "$@")"
    "$@"
}

ask() {
    local answer
    if [ "$yes" = 1 ]; then
        say "$1 [y/N] yes (--yes)"
        return 0
    fi
    if [ ! -t 0 ]; then
        say "$1 [y/N] no (there is no terminal to answer on)"
        return 1
    fi
    read -r -p "$1 [y/N] " answer
    case "$answer" in
        [yY]*) return 0 ;;
        *) return 1 ;;
    esac
}

is_known_file() {
    local f
    for f in "${KNOWN_FILES[@]}"; do
        [ "$f" = "$1" ] && return 0
    done
    return 1
}

# True for a directory the installer may have created, which is an ancestor of one of its paths.
is_known_dir() {
    local f
    for f in "${KNOWN_FILES[@]}" "$share_dir/x"; do
        case "$f" in
            "$1"/*) return 0 ;;
        esac
    done
    return 1
}

# --- What is there --------------------------------------------------------

files=()
trees=()
dirs=()
packages=()
if [ -f "$root$manifest" ]; then
    say "Reading $root$manifest."
    while read -r kind path; do
        case "$kind" in
            file)
                is_known_file "$path" || fail "The manifest names $path, which the installer never writes."
                files+=("$path")
                ;;
            tree)
                [ "$path" = "$share_dir" ] || fail "The manifest names the folder $path, which the installer never writes."
                trees+=("$path")
                ;;
            dir)
                is_known_dir "$path" || fail "The manifest names the folder $path, which the installer never creates."
                dirs+=("$path")
                ;;
            package)
                [ "$path" = cage ] || fail "The manifest names the package $path, which the installer never installs."
                packages+=("$path")
                ;;
        esac
    done <"$root$manifest"
else
    say "There is no $root$manifest, so only the files the installer writes are removed, where they exist."
    files=("${KNOWN_FILES[@]}")
fi

present=()
for f in "${files[@]}"; do
    if [ -e "$root$f" ] || [ -L "$root$f" ]; then
        present+=("$f")
    fi
done
present_trees=()
for t in "${trees[@]}"; do
    if [ -d "$root$t" ]; then
        present_trees+=("$t")
    fi
done
if [ -n "$root" ]; then
    packages=()
fi

if [ "${#present[@]}" -eq 0 ] && [ "${#present_trees[@]}" -eq 0 ] && [ "${#packages[@]}" -eq 0 ]; then
    say "The meerora session is not installed below $root$prefix, so there is nothing to remove."
    exit 0
fi

say "These paths go:"
for f in "${present[@]}"; do
    say "  $root$f"
done
for t in "${present_trees[@]}"; do
    say "  $root$t with everything in it"
done
for d in "${dirs[@]}"; do
    say "  $root$d once it is empty"
done
for p in "${packages[@]}"; do
    say "  the package $p, after a separate question"
done
say "Your data in ~/.local/share/meerora and the directories of your agents stay."

# --- Removing -------------------------------------------------------------

if [ "$dry" = 0 ] && [ -z "$root" ] && [ "$(id -u)" -ne 0 ]; then
    fail "The files belong to root, so run it with sudo scripts/uninstall-session.sh."
fi
if [ "$dry" = 0 ] && ! ask "Remove the meerora session?"; then
    say "Nothing was removed."
    exit 0
fi

for f in "${present[@]}"; do
    run rm -f "$root$f"
done
for t in "${present_trees[@]}"; do
    run rm -rf "$root$t"
done
for d in "${dirs[@]}"; do
    if [ "$dry" = 1 ]; then
        say "  (dry run) would remove $root$d if it is empty."
    elif [ -d "$root$d" ] && [ -z "$(ls -A "$root$d")" ]; then
        run rmdir "$root$d"
    fi
done
for p in "${packages[@]}"; do
    if [ "$dry" = 1 ]; then
        say "  (dry run) would ask whether to remove $p and then run dnf remove -y $p."
    elif ask "The installer also installed $p with dnf. Remove it too?"; then
        run dnf remove -y "$p"
    else
        say "  $p stays installed."
    fi
done

if [ "$dry" = 1 ]; then
    say "This was a dry run, so nothing changed."
else
    say "The meerora session is removed. The login screen no longer offers it."
fi
