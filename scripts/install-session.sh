#!/usr/bin/env bash
# install-session.sh installs meerora as a login session next to KDE Plasma (D14, D15).
#
# Run it from the repository with sudo:
#   sudo scripts/install-session.sh             install or update
#   scripts/install-session.sh --dry-run        print every step and change nothing
#
# It builds the release binaries as the user who called sudo, installs them
# with the session launcher to /usr/local/bin, the shipped assets to
# /usr/local/share/meerora and the session entry to
# /usr/share/wayland-sessions/meerora.desktop, and the logo as the icon of an
# app entry, so the task bar shows it next to the app id meerora. It installs
# cage with dnf after asking. Running it again updates everything in place.
# scripts/uninstall-session.sh removes exactly what it added.

set -euo pipefail

repo="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
prefix=/usr/local
root=""
dry=0
yes=0
binaries=""

usage() {
    cat <<'EOF'
Usage: sudo scripts/install-session.sh [options]

Options:
  --dry-run        Print every step and change nothing. Needs no sudo.
  --yes            Answer yes to every question, which installs cage when it is missing.
  --prefix DIR     Install below DIR instead of /usr/local.
  --root DIR       Install into DIR as a staging root, for packaging and tests.
                   Needs no sudo and installs no packages.
  --binaries DIR   Install meerora, meerora-agentd and meerora-mcp from DIR instead of building them.
  -h, --help       Show this help.
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
        --binaries)
            binaries="${2:?--binaries needs a directory}"
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

BINARIES=(meerora meerora-agentd meerora-mcp)
ASSET_PARTS=(spaces agents scenarios)
# Runtime files of a data dir that never belong into the shipped assets.
ASSET_EXCLUDES=(.git logs artifacts state.json state.json.tmp grants.toml settings.toml layouts.json)

bin_dir="$prefix/bin"
share_dir="$prefix/share/meerora"
session_file=/usr/share/wayland-sessions/meerora.desktop
target_file="$prefix/lib/systemd/user/meerora-session.target"
portal_file="$prefix/share/xdg-desktop-portal/meerora-portals.conf"
app_file="$prefix/share/applications/meerora.desktop"
# The sizes of assets/logo/meerora-logo-N.png that go into the hicolor icon theme.
ICON_SIZES=(16 32 48 64 128 256 512)
icon_files=()
for s in "${ICON_SIZES[@]}"; do
    icon_files+=("$prefix/share/icons/hicolor/${s}x$s/apps/meerora.png")
done
manifest="$share_dir/install-manifest.txt"
INSTALLED_FILES=("$bin_dir/meerora" "$bin_dir/meerora-agentd" "$bin_dir/meerora-mcp" "$bin_dir/meerora-session"
    "$session_file" "$target_file" "$portal_file" "$app_file" "${icon_files[@]}")

done_lines=()

say() {
    printf '%s\n' "$*"
}

step() {
    printf '\n== %s\n' "$*"
}

fail() {
    printf 'install-session.sh stopped. %s\n' "$*" >&2
    exit 1
}

quoted() {
    local out=() a
    for a in "$@"; do
        out+=("$(printf '%q' "$a")")
    done
    printf '%s' "${out[*]}"
}

# Runs a command, or only prints it with --dry-run.
run() {
    if [ "$dry" = 1 ]; then
        say "  (dry run) \$ $(quoted "$@")"
        return 0
    fi
    say "  \$ $(quoted "$@")"
    "$@"
}

# Asks a yes or no question. The default is no.
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

# Writes standard input to a file below the root with the given mode, or prints it with --dry-run.
put() {
    local path="$1" mode="$2" content dest
    content="$(cat)"
    dest="$root$path"
    if [ "$dry" = 1 ]; then
        say "  (dry run) would write $dest with mode $mode and this content"
        printf '%s\n' "$content" | sed 's/^/    | /'
        return 0
    fi
    make_dirs "$(dirname "$path")"
    printf '%s\n' "$content" >"$dest.new"
    chmod "$mode" "$dest.new"
    mv -f "$dest.new" "$dest"
    say "  Wrote $dest."
    done_lines+=("$dest")
}

# Creates a directory below the root with its missing parents.
make_dirs() {
    if [ ! -d "$root$1" ]; then
        run install -d -m 0755 "$root$1"
    fi
}

# The directories the installer has to create for its files, deepest first.
# The uninstaller removes them again when they are empty.
missing_dirs() {
    local path dir seen=()
    for path in "${INSTALLED_FILES[@]}" "$share_dir/x"; do
        dir="$(dirname "$path")"
        while [ "$dir" != / ] && [ ! -d "$root$dir" ]; do
            seen+=("$dir")
            dir="$(dirname "$dir")"
        done
    done
    if [ "${#seen[@]}" -gt 0 ]; then
        printf '%s\n' "${seen[@]}" | awk '{ print length($0) " " $0 }' | sort -rn | cut -d' ' -f2- | uniq
    fi
}

# --- Checks ---------------------------------------------------------------

step "Checking"
if [ "$dry" = 1 ]; then
    say "This is a dry run. Nothing changes."
fi
if [ "$dry" = 0 ] && [ -z "$root" ] && [ "$(id -u)" -ne 0 ]; then
    fail "It installs to $prefix and /usr/share, so run it with sudo scripts/install-session.sh."
fi
build_user=""
if [ -z "$binaries" ]; then
    if [ "$(id -u)" -eq 0 ]; then
        build_user="${SUDO_USER:-}"
        if [ -z "$build_user" ] || [ "$build_user" = root ]; then
            fail "Cargo must not build as root. Run the script with sudo from your own account, or pass --binaries DIR."
        fi
    else
        build_user="$(id -un)"
    fi
    say "The release binaries are built as $build_user."
else
    say "The binaries come from $binaries."
fi
if [ -d "$root$share_dir" ] && [ ! -f "$root$manifest" ]; then
    fail "$root$share_dir exists, but this installer did not make it, so the script leaves it alone."
fi
say "The repository is $repo."
if [ -n "$root" ]; then
    say "Everything goes below the staging root $root."
fi

# Keeps what an earlier run recorded, which is whether it installed cage and which directories it created.
previous_dirs=()
cage_ours=0
if [ -f "$root$manifest" ]; then
    while read -r kind path; do
        case "$kind" in
            dir) previous_dirs+=("$path") ;;
            package) [ "$path" = cage ] && cage_ours=1 ;;
        esac
    done <"$root$manifest"
fi

# --- cage -----------------------------------------------------------------

step "cage, the kiosk compositor the session runs in"
if command -v cage >/dev/null 2>&1; then
    say "cage is installed at $(command -v cage)."
elif [ -n "$root" ]; then
    say "cage is missing on this system. With --root the script installs no packages."
elif ! command -v dnf >/dev/null 2>&1; then
    say "cage is missing and dnf is not available. Install cage with your package manager before you log in to meerora."
elif [ "$dry" = 1 ]; then
    say "cage is missing. The script would ask whether to install it and then run dnf install -y cage."
elif ask "cage is not installed. The meerora session runs inside it. Install it now with dnf?"; then
    run dnf install -y cage
    cage_ours=1
    done_lines+=("the package cage (dnf)")
else
    say "cage stays missing. The session cannot start until you run sudo dnf install cage."
fi

# --- Build ----------------------------------------------------------------

step "Release binaries"
if [ -n "$binaries" ]; then
    source_dir="$(realpath -m "$binaries")"
    say "Using the binaries in $source_dir instead of building."
else
    cargo_args=(build --release -p meerora -p meerora-agentd -p meerora-mcp)
    if [ "$(id -u)" -eq 0 ]; then
        user_home="$(getent passwd "$build_user" | cut -d: -f6)"
        as_user=(sudo -u "$build_user" -H env -C "$repo" "PATH=$user_home/.cargo/bin:$PATH")
    else
        as_user=(env -C "$repo")
    fi
    run "${as_user[@]}" cargo "${cargo_args[@]}"
    if [ "$dry" = 1 ]; then
        source_dir="$repo/target/release"
        say "  (dry run) would take the binaries from the target directory that cargo metadata names, usually $source_dir."
    else
        target_dir="$("${as_user[@]}" cargo metadata --no-deps --format-version 1 | grep -o '"target_directory":"[^"]*"' | cut -d'"' -f4)"
        source_dir="${target_dir:-$repo/target}/release"
    fi
fi
if [ "$dry" = 0 ]; then
    for b in "${BINARIES[@]}"; do
        [ -x "$source_dir/$b" ] || fail "There is no $source_dir/$b."
    done
fi

# --- Manifest -------------------------------------------------------------

step "Manifest for the uninstaller"
mapfile -t new_dirs < <(missing_dirs)
{
    say "# Written by scripts/install-session.sh. scripts/uninstall-session.sh removes these paths."
    say "# It removes every file and every tree with its contents, every dir once it is empty,"
    say "# and it offers to remove every package with dnf."
    for f in "${INSTALLED_FILES[@]}"; do
        say "file $f"
    done
    say "tree $share_dir"
    if [ "${#previous_dirs[@]}" -gt 0 ] || [ "${#new_dirs[@]}" -gt 0 ]; then
        printf '%s\n' "${previous_dirs[@]}" "${new_dirs[@]}" | awk 'NF { print length($0) " " $0 }' | sort -rn | cut -d' ' -f2- | uniq |
            sed 's/^/dir /'
    fi
    if [ "$cage_ours" = 1 ]; then
        say "package cage"
    fi
} | put "$manifest" 0644

# --- Files ----------------------------------------------------------------

step "Binaries and the session launcher in $root$bin_dir"
make_dirs "$bin_dir"
for b in "${BINARIES[@]}"; do
    run install -m 0755 "$source_dir/$b" "$root$bin_dir/$b"
    done_lines+=("$root$bin_dir/$b")
done
run install -m 0755 "$repo/scripts/meerora-session" "$root$bin_dir/meerora-session"
done_lines+=("$root$bin_dir/meerora-session")

step "Shipped assets in $root$share_dir"
excludes=()
for e in "${ASSET_EXCLUDES[@]}"; do
    excludes+=("--exclude=$e")
done
for part in "${ASSET_PARTS[@]}"; do
    [ -d "$repo/assets/$part" ] || fail "There is no $repo/assets/$part."
done
# An install from before rooms has space/ instead of spaces/, which goes too.
for part in "${ASSET_PARTS[@]}" space; do
    if [ -e "$root$share_dir/$part" ]; then
        run rm -rf "$root$share_dir/$part"
    fi
done
if [ "$dry" = 1 ]; then
    say "  (dry run) would copy ${ASSET_PARTS[*]} from $repo/assets without ${ASSET_EXCLUDES[*]}."
else
    say "  Copying ${ASSET_PARTS[*]} from $repo/assets without ${ASSET_EXCLUDES[*]}."
    tar -C "$repo/assets" "${excludes[@]}" -cf - "${ASSET_PARTS[@]}" | tar -C "$root$share_dir" --no-same-owner --no-same-permissions -xf -
fi
make_dirs "$share_dir/licenses"
run install -m 0644 "$repo/LICENSE" "$repo/assets/fonts/OFL-BricolageGrotesque.txt" "$repo/assets/fonts/OFL-IBMPlexMono.txt" \
    "$root$share_dir/licenses/"
run chmod -R u=rwX,go=rX "$root$share_dir"
done_lines+=("$root$share_dir")

step "Session entry for the login screen"
put "$session_file" 0644 <<EOF
[Desktop Entry]
Type=Application
Name=meerora
Comment=LLM agents as figures in one shared space, in the cage kiosk compositor
Exec=$bin_dir/meerora-session
TryExec=$bin_dir/meerora-session
Icon=meerora
DesktopNames=meerora
Keywords=meerora;agents;wayland;
EOF

step "The logo as an icon and the app entry, so the task bar shows it for the app id meerora"
for i in "${!ICON_SIZES[@]}"; do
    make_dirs "$(dirname "${icon_files[$i]}")"
    run install -m 0644 "$repo/assets/logo/meerora-logo-${ICON_SIZES[$i]}.png" "$root${icon_files[$i]}"
    done_lines+=("$root${icon_files[$i]}")
done
put "$app_file" 0644 <<EOF
[Desktop Entry]
Type=Application
Name=meerora
Comment=LLM agents as figures in one shared space
Exec=$bin_dir/meerora
TryExec=$bin_dir/meerora
Icon=meerora
Terminal=false
Categories=Utility;
StartupWMClass=meerora
EOF

step "systemd user target, so services of graphical-session.target follow the session"
put "$target_file" 0644 <<'EOF'
# Started by meerora-session inside cage and stopped when meerora quits.
[Unit]
Description=meerora session
Documentation=man:systemd.special(7)
BindsTo=graphical-session.target
Wants=graphical-session-pre.target
After=graphical-session-pre.target
EOF

step "Portal backends for XDG_CURRENT_DESKTOP=meerora"
put "$portal_file" 0644 <<'EOF'
# xdg-desktop-portal reads this file in the meerora session. meerora reuses
# KDE's portal backends and falls back to the GTK ones (D15, phase 3).
[preferred]
default=kde;gtk;
EOF

# --- Summary --------------------------------------------------------------

step "Summary"
if [ "$dry" = 1 ]; then
    say "This was a dry run, so nothing changed. Run sudo scripts/install-session.sh to install."
    exit 0
fi
say "Installed or updated these paths:"
for line in "${done_lines[@]}"; do
    say "  $line"
done
cat <<EOF

To try meerora in a window first, run meerora --windowed, or meerora-session --nested for the whole session in a window.
To use the session, log out of KDE and pick meerora in the session menu of the login screen.
In the session, Log out in the space header or Ctrl+Alt+Delete brings the login screen back, where you pick Plasma again.
To remove it, run sudo scripts/uninstall-session.sh.
EOF
