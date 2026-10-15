#!/bin/sh
# The meerora install script, served at https://meerora.com/install.sh
#
#   curl -fsSL https://meerora.com/install.sh | sh
#
# It installs meerora for you alone, without root:
#
#   1. It downloads the prebuilt release from
#      https://github.com/ribalba/meerora/releases, checks its SHA-256 and
#      unpacks it to ~/.local/share/meerora-app/<version>/.
#   2. It links meerora, meerora-agentd and meerora-mcp into ~/.local/bin and
#      adds the icon and an entry to your app menu.
#   3. It creates your data dir ~/.local/share/meerora with
#      meerora-agentd init when you have none yet. It never touches an
#      existing data dir.
#   4. It starts meerora in a window.
#
# It never runs sudo. When a package is missing, it prints the command that
# installs it. Running it again updates meerora in place. When there is no
# release yet, it offers to build meerora from source, which takes 10 to 20
# minutes. At the end it prints which paths to remove to uninstall meerora.
#
# Options (pass them with curl -fsSL https://meerora.com/install.sh | sh -s -- --from-source):
#   --from-source            Build meerora from source instead of downloading the release.
#   -h, --help               Show this help.
#
# Environment:
#   MEERORA_VERSION=v0.1.0   Install this release tag instead of the latest release.
#   MEERORA_TARBALL=PATH     Install this tarball, a path or an http(s) URL, instead of
#                            the release download. Its checksum must be at PATH.sha256.
#   MEERORA_NO_LAUNCH=1      Do not start meerora at the end.
#
# Everything runs inside main, which is called on the last line, so a download
# that breaks off half way runs nothing.

set -eu

REPO_URL="https://github.com/ribalba/meerora"
ASSET="meerora-x86_64-linux.tar.gz"
TOP="meerora-x86_64-linux"
BINARIES="meerora meerora-agentd meerora-mcp"
MIN_RUST="1.95"
MARKER=".meerora-install"
DESKTOP_MARK="# Written by the meerora install script, https://meerora.com/install.sh"

# What provides a library, a program or a build tool. The columns are the
# name the script checks, then the packages for dnf, apt, pacman and zypper.
# A comma joins several packages.
PACKAGES='
bwrap                   bubblewrap              bubblewrap              bubblewrap          bubblewrap
git                     git                     git                     git                 git
libxkbcommon.so.0       libxkbcommon            libxkbcommon0           libxkbcommon        libxkbcommon0
libstdc++.so.6          libstdc++               libstdc++6              gcc-libs            libstdc++6
libgcc_s.so.1           libgcc                  libgcc-s1               gcc-libs            libgcc_s1
libwayland-client.so.0  libwayland-client       libwayland-client0      wayland             libwayland-client0
libX11.so.6             libX11                  libx11-6                libx11              libX11-6
libX11-xcb.so.1         libX11-xcb              libx11-xcb1             libx11              libX11-xcb1
libxcb.so.1             libxcb                  libxcb1                 libxcb              libxcb1
libXcursor.so.1         libXcursor              libxcursor1             libxcursor          libXcursor1
libXi.so.6              libXi                   libxi6                  libxi               libXi6
libxkbcommon-x11.so.0   libxkbcommon-x11        libxkbcommon-x11-0      libxkbcommon-x11    libxkbcommon-x11-0
libvulkan.so.1          vulkan-loader,mesa-vulkan-drivers,mesa-libEGL,libwayland-egl  libvulkan1,mesa-vulkan-drivers,libegl1,libwayland-egl1  vulkan-icd-loader,mesa  libvulkan1,Mesa-libEGL1,libwayland-egl1
cc                      gcc                     gcc                     gcc                 gcc
c++                     gcc-c++                 g++                     gcc                 gcc-c++
make                    make                    make                    make                make
cmake                   cmake                   cmake                   cmake               cmake
pkg-config              pkgconf-pkg-config      pkg-config              pkgconf             pkg-config
libclang                clang,clang-devel       clang,libclang-dev      clang               clang,clang-devel
xkbcommon-dev           libxkbcommon-devel      libxkbcommon-dev        libxkbcommon        libxkbcommon-devel
wayland-dev             wayland-devel           libwayland-dev          wayland             wayland-devel
'

usage() {
    cat <<'EOF'
Installs meerora for the current user, without root.

  curl -fsSL https://meerora.com/install.sh | sh
  curl -fsSL https://meerora.com/install.sh | sh -s -- --from-source

Options:
  --from-source           Build meerora from source instead of downloading the release.
  -h, --help              Show this help.

Environment:
  MEERORA_VERSION=v0.1.0  Install this release tag instead of the latest release.
  MEERORA_TARBALL=PATH    Install this tarball (a path or an http(s) URL) instead of the
                          release download. Its checksum must be at PATH.sha256.
  MEERORA_NO_LAUNCH=1     Do not start meerora at the end.
EOF
}

say() {
    printf '%s\n' "$*"
}

step() {
    printf '\n== %s\n' "$*"
}

fail() {
    printf '\nThe meerora install stopped. %s\n' "$*" >&2
    exit 1
}

has() {
    command -v "$1" >/dev/null 2>&1
}

# Asks a yes or no question on the terminal, because stdin is the script
# itself when it is piped into sh. The answer is no without a terminal.
ask() {
    if ! (: </dev/tty) 2>/dev/null; then
        say "$1 [y/N] no, because there is no terminal to answer on."
        return 1
    fi
    printf '%s [y/N] ' "$1"
    answer=""
    read -r answer </dev/tty || answer=""
    case "$answer" in
        [yY]*) return 0 ;;
        *) return 1 ;;
    esac
}

# Prints a path so that a shell reads it back as one word.
shell_quote() {
    case "$1" in
        *[!A-Za-z0-9/._+-]*) printf "'%s'" "$(printf '%s' "$1" | sed "s/'/'\\\\''/g")" ;;
        *) printf '%s' "$1" ;;
    esac
}

# --- Packages -------------------------------------------------------------

detect_package_manager() {
    if has dnf; then
        pm=dnf
    elif has apt-get; then
        pm=apt
    elif has pacman; then
        pm=pacman
    elif has zypper; then
        pm=zypper
    else
        pm=""
    fi
}

# Prints the packages that provide NAME for this package manager, or NAME
# itself when the table does not know it.
packages_for() {
    case "$pm" in
        dnf) column=2 ;;
        apt) column=3 ;;
        pacman) column=4 ;;
        zypper) column=5 ;;
        *) column=0 ;;
    esac
    found="$(printf '%s\n' "$PACKAGES" | awk -v name="$1" -v col="$column" '$1 == name && col > 0 { print $col }' | tr ',' ' ')"
    printf '%s\n' "${found:-$1}"
}

# Adds the packages for NAME to the list in the variable LIST, once each.
need() {
    list_name="$1"
    eval "list=\${$list_name}"
    for p in $(packages_for "$2"); do
        case " $list " in
            *" $p "*) ;;
            *) list="${list:+$list }$p" ;;
        esac
    done
    eval "$list_name=\$list"
}

# Prints the command that installs the given packages.
install_command() {
    case "$pm" in
        dnf) say "sudo dnf install $*" ;;
        apt) say "sudo apt install $*" ;;
        pacman) say "sudo pacman -S --needed $*" ;;
        zypper) say "sudo zypper install $*" ;;
        *) say "Install these with your package manager: $*" ;;
    esac
}

# Whether the dynamic loader finds the library SONAME.
have_lib() {
    for ldconfig in ldconfig /sbin/ldconfig /usr/sbin/ldconfig; do
        if "$ldconfig" -p 2>/dev/null | awk -v name="$1" '$1 == name && /x86-64/ { found = 1 } END { exit !found }'; then
            return 0
        fi
    done
    for dir in /usr/lib64 /lib64 /usr/lib/x86_64-linux-gnu /lib/x86_64-linux-gnu /usr/lib; do
        [ -e "$dir/$1" ] && return 0
    done
    return 1
}

# --- Checks ---------------------------------------------------------------

check_platform() {
    os="$(uname -s)"
    arch="$(uname -m)"
    if [ "$os" != Linux ] || [ "$arch" != x86_64 ]; then
        fail "meerora runs on Linux on x86_64 only, and this is $os on $arch. The README at $REPO_URL says how to build it yourself."
    fi
    if [ ! -e /lib64/ld-linux-x86-64.so.2 ]; then
        fail "meerora needs a Linux with glibc, and this system has none (for example Alpine, which uses musl)."
    fi
}

# Looks for what meerora needs at run time that does not come with it.
check_system() {
    if has bwrap || [ -x /usr/bin/bwrap ]; then
        say "bubblewrap (bwrap) is installed. Agents run inside it."
    else
        say "bubblewrap (bwrap) is missing. Every agent runs inside this sandbox, so agents cannot work until it is installed."
        need required bwrap
    fi
    if has git; then
        say "git is installed."
    else
        say "git is missing. meerora keeps every agent's directory as a git repository, so you can review its changes."
        need recommended git
    fi
    if [ -n "${WAYLAND_DISPLAY:-}" ]; then
        session=wayland
        say "This is a Wayland session ($WAYLAND_DISPLAY)."
    elif [ -n "${DISPLAY:-}" ]; then
        session=x11
        say "This is an X11 session ($DISPLAY)."
    else
        session=none
        say "There is no graphical session here, because neither WAYLAND_DISPLAY nor DISPLAY is set. meerora gets installed but does not start now."
    fi
}

# Checks the libraries of the unpacked binaries in DIR: the ones they link
# and the ones they load at run time for the window and the GPU.
check_libraries() {
    if ! has ldd; then
        say "ldd is missing, so the script cannot check the libraries."
        return 0
    fi
    for b in $BINARIES; do
        if ldd "$1/bin/$b" 2>&1 | grep -q "version .GLIBC_"; then
            fail "The glibc of this system is older than the prebuilt binaries need (Ubuntu 24.04, Fedora 40 and Debian 13 are new enough). To build meerora from source instead, run curl -fsSL https://meerora.com/install.sh | sh -s -- --from-source"
        fi
    done
    libs_missing=0
    linked_missing="$(for b in $BINARIES; do ldd "$1/bin/$b" 2>/dev/null | awk '/=> not found/ { print $1 }'; done | sort -u)"
    for lib in $linked_missing; do
        say "The library $lib is missing."
        need required "$lib"
        libs_missing=1
    done
    case "$session" in
        x11) loaded="libX11.so.6 libX11-xcb.so.1 libxcb.so.1 libXcursor.so.1 libXi.so.6 libxkbcommon-x11.so.0" ;;
        *) loaded="libwayland-client.so.0" ;;
    esac
    for lib in $loaded; do
        if ! have_lib "$lib"; then
            say "The library $lib is missing. meerora loads it to open its window."
            need required "$lib"
            libs_missing=1
        fi
    done
    if have_lib libvulkan.so.1 || have_lib libEGL.so.1; then
        say "Vulkan or EGL is there for the GPU."
    else
        say "Neither Vulkan (libvulkan.so.1) nor EGL (libEGL.so.1) is installed. meerora draws its space on the GPU with one of them."
        need required libvulkan.so.1
        libs_missing=1
    fi
    if [ "$libs_missing" = 0 ]; then
        say "Every library meerora needs is there."
    fi
}

cargo_version_ok() {
    version_text="$(cargo --version 2>/dev/null | awk '{ print $2 }')"
    major="${version_text%%.*}"
    rest="${version_text#*.}"
    minor="${rest%%.*}"
    want_minor="${MIN_RUST#*.}"
    case "$major$minor" in
        '' | *[!0-9]*) return 1 ;;
    esac
    [ "$major" -gt 1 ] || { [ "$major" -eq 1 ] && [ "$minor" -ge "$want_minor" ]; }
}

have_libclang() {
    if [ -n "${LIBCLANG_PATH:-}" ]; then
        for f in "$LIBCLANG_PATH"/libclang.so* "$LIBCLANG_PATH"/libclang-[0-9]*.so*; do
            [ -e "$f" ] && return 0
        done
    fi
    if has llvm-config; then
        dir="$(llvm-config --libdir 2>/dev/null || true)"
        for f in "$dir"/libclang.so* "$dir"/libclang-[0-9]*.so*; do
            [ -e "$f" ] && return 0
        done
    fi
    for f in /usr/lib64/libclang.so* /usr/lib64/libclang-[0-9]*.so* /usr/lib/libclang.so* \
        /usr/lib/x86_64-linux-gnu/libclang-[0-9]*.so* /usr/lib/llvm-*/lib/libclang.so* \
        /usr/lib/llvm-*/lib/libclang-[0-9]*.so* /usr/lib64/llvm*/lib*/libclang.so*; do
        [ -e "$f" ] && return 0
    done
    return 1
}

# Checks what a build from source needs and stops with the commands that
# install the missing parts.
check_build_tools() {
    tools=""
    rust_note=""
    if ! has cargo && [ -x "$HOME/.cargo/bin/cargo" ]; then
        PATH="$HOME/.cargo/bin:$PATH"
        export PATH
    fi
    if ! has cargo; then
        rust_note="Rust is missing. Install Rust $MIN_RUST or newer with rustup from https://rustup.rs:"
    elif ! cargo_version_ok; then
        rust_note="$(cargo --version) is older than Rust $MIN_RUST. Update it with rustup update, or install rustup from https://rustup.rs:"
    else
        say "$(cargo --version) is new enough."
    fi
    has git || need tools git
    has cc || has gcc || need tools cc
    has c++ || has g++ || need tools c++
    has make || need tools make
    has cmake || need tools cmake
    if has pkg-config || has pkgconf; then
        pkg_config="$(command -v pkg-config || command -v pkgconf)"
        "$pkg_config" --exists xkbcommon 2>/dev/null || need tools xkbcommon-dev
        "$pkg_config" --exists wayland-client 2>/dev/null || need tools wayland-dev
    else
        need tools pkg-config
        need tools xkbcommon-dev
        need tools wayland-dev
    fi
    have_libclang || need tools libclang
    if [ -z "$tools" ] && [ -z "$rust_note" ]; then
        say "Every build tool is there."
        return 0
    fi
    if [ -n "$tools" ]; then
        say "These build tools are missing. Install them with:"
        say "  $(install_command "$tools")"
    fi
    if [ -n "$rust_note" ]; then
        say "$rust_note"
        say "  curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh"
    fi
    fail "Run this script again with --from-source once the build tools are installed."
}

# --- Getting the tarball --------------------------------------------------

# Downloads URL to FILE and prints the HTTP status, 000 when there was no answer.
fetch() {
    if [ "${1#https://}" != "$1" ]; then
        curl -sSL --retry 3 --proto '=https' --tlsv1.2 -o "$2" -w '%{http_code}' "$1" || true
    else
        curl -sSL --retry 3 -o "$2" -w '%{http_code}' "$1" || true
    fi
}

# Sets tarball and checksum to the downloaded release, or builds from source
# when there is no release and the user agrees.
download_release() {
    has curl || fail "curl is missing. Install it and run the script again."
    if [ -n "${MEERORA_VERSION:-}" ]; then
        base="$REPO_URL/releases/download/$MEERORA_VERSION"
        what="meerora $MEERORA_VERSION"
    else
        base="$REPO_URL/releases/latest/download"
        what="the latest meerora release"
    fi
    say "Downloading $what from $base/$ASSET."
    tarball="$work/$ASSET"
    checksum="$work/$ASSET.sha256"
    code="$(fetch "$base/$ASSET" "$tarball")"
    case "$code" in
        200) ;;
        404)
            if [ -n "${MEERORA_VERSION:-}" ]; then
                say "There is no release $MEERORA_VERSION with $ASSET on GitHub."
            else
                say "There is no meerora release on GitHub yet."
            fi
            say "The script can build meerora from source instead. That takes 10 to 20 minutes and about 3 GB of disk space, and it needs Rust and a few build tools."
            if ask "Build meerora from source now?"; then
                build_from_source
                return 0
            fi
            say "Nothing was installed. To build from source later, run:"
            say "  curl -fsSL https://meerora.com/install.sh | sh -s -- --from-source"
            exit 1
            ;;
        000) fail "The download of $base/$ASSET got no answer. Check the network and run the script again." ;;
        *) fail "The download of $base/$ASSET failed with HTTP status $code. Nothing was installed." ;;
    esac
    code="$(fetch "$base/$ASSET.sha256" "$checksum")"
    [ "$code" = 200 ] || fail "The download of $base/$ASSET.sha256 failed with HTTP status $code, so the tarball cannot be checked. Nothing was installed."
}

# Sets tarball and checksum to MEERORA_TARBALL and its .sha256.
use_given_tarball() {
    case "$MEERORA_TARBALL" in
        http://* | https://*)
            has curl || fail "curl is missing. Install it and run the script again."
            say "Downloading $MEERORA_TARBALL."
            tarball="$work/$ASSET"
            checksum="$work/$ASSET.sha256"
            code="$(fetch "$MEERORA_TARBALL" "$tarball")"
            [ "$code" = 200 ] || fail "The download of $MEERORA_TARBALL failed with HTTP status $code."
            code="$(fetch "$MEERORA_TARBALL.sha256" "$checksum")"
            [ "$code" = 200 ] || fail "The download of $MEERORA_TARBALL.sha256 failed with HTTP status $code."
            ;;
        *)
            tarball="$MEERORA_TARBALL"
            checksum="$MEERORA_TARBALL.sha256"
            [ -f "$tarball" ] || fail "There is no file $tarball."
            [ -f "$checksum" ] || fail "There is no checksum file $checksum next to the tarball."
            say "Using the tarball $tarball."
            ;;
    esac
}

# Clones meerora into a folder of its own, builds it with
# scripts/package-release.sh and sets tarball and checksum to the result.
build_from_source() {
    step "Building meerora from source"
    say "This takes 10 to 20 minutes and about 3 GB of disk space in $app_root/source."
    check_build_tools
    src="$app_root/source"
    ref="${MEERORA_VERSION:-}"
    if [ -d "$src/.git" ]; then
        say "Updating the source in $src."
        git -C "$src" fetch --depth 1 origin "${ref:-HEAD}" || fail "git could not update $src."
        git -C "$src" checkout --quiet --force --detach FETCH_HEAD || fail "git could not update $src."
    else
        rm -rf "$src"
        say "Cloning $REPO_URL into $src."
        if [ -n "$ref" ]; then
            git clone --depth 1 --branch "$ref" "$REPO_URL.git" "$src" || fail "git could not clone $REPO_URL."
        else
            git clone --depth 1 "$REPO_URL.git" "$src" || fail "git could not clone $REPO_URL."
        fi
    fi
    if [ -n "$ref" ]; then
        sh "$src/scripts/package-release.sh" --out "$src/target/package" --version "$ref" || fail "The build failed, see the output above."
    else
        sh "$src/scripts/package-release.sh" --out "$src/target/package" || fail "The build failed, see the output above."
    fi
    tarball="$src/target/package/$ASSET"
    checksum="$tarball.sha256"
}

# Prints the SHA-256 of FILE. verify_checksum makes sure one of the tools exists.
sha256_of() {
    if has sha256sum; then
        sha256sum "$1" | awk '{ print $1 }'
    elif has shasum; then
        shasum -a 256 "$1" | awk '{ print $1 }'
    else
        openssl dgst -sha256 -r "$1" | awk '{ print $1 }'
    fi
}

verify_checksum() {
    if ! has sha256sum && ! has shasum && ! has openssl; then
        fail "There is no sha256sum, shasum or openssl to check the download with."
    fi
    expected="$(awk 'NR == 1 { print tolower($1) }' "$checksum")"
    if [ "${#expected}" -ne 64 ] || [ -n "$(printf '%s' "$expected" | tr -d '0-9a-f')" ]; then
        fail "$checksum does not hold a SHA-256. Nothing was installed."
    fi
    actual="$(sha256_of "$tarball")"
    if [ "$actual" != "$expected" ]; then
        fail "The SHA-256 of the tarball is $actual, but $checksum says $expected. The download is broken or was changed, so nothing was installed."
    fi
    say "The SHA-256 matches ($actual)."
}

# --- Installing -----------------------------------------------------------

# Creates the app folder with its marker, or makes sure an existing one is ours.
prepare_app_root() {
    if [ -d "$app_root" ] && [ ! -f "$app_root/$MARKER" ] && [ -n "$(ls -A "$app_root")" ]; then
        fail "$app_root exists, but this script did not create it, so it leaves it alone."
    fi
    mkdir -p "$app_root"
    if [ ! -f "$app_root/$MARKER" ]; then
        say "This folder was created by the meerora install script, https://meerora.com/install.sh." >"$app_root/$MARKER"
    fi
    for old in "$app_root"/.work.*; do
        [ -d "$old" ] && rm -rf "$old"
    done
    work="$(mktemp -d "$app_root/.work.XXXXXX")"
}

# Unpacks the tarball and sets new_dir and version.
unpack() {
    mkdir "$work/unpack"
    tar -xzf "$tarball" -C "$work/unpack" || fail "The tarball $tarball cannot be unpacked."
    new_dir="$work/unpack/$TOP"
    for b in $BINARIES; do
        [ -x "$new_dir/bin/$b" ] || fail "The tarball has no $TOP/bin/$b."
    done
    [ -d "$new_dir/share/meerora/spaces" ] || fail "The tarball has no $TOP/share/meerora/spaces."
    [ -f "$new_dir/VERSION" ] || fail "The tarball has no $TOP/VERSION."
    version="$(sed -n '1p' "$new_dir/VERSION" | tr -cd 'A-Za-z0-9._+-')"
    case "$version" in
        '' | .* | current | source) version="release-$version" ;;
    esac
    say "Unpacked meerora $version."
}

# Moves the new version into place, points current at it and keeps only
# the version that was current before.
install_version() {
    app_dir="$app_root/$version"
    previous=""
    if [ -L "$app_root/current" ]; then
        previous="$(readlink "$app_root/current")"
    fi
    if [ -e "$app_dir" ]; then
        say "Replacing $app_dir, which has the same version."
        mv "$app_dir" "$work/replaced"
    fi
    mv "$new_dir" "$app_dir"
    ln -sfn "$version" "$app_root/current"
    say "Installed meerora $version to $app_dir."
    for dir in "$app_root"/*; do
        if [ ! -d "$dir" ] || [ -L "$dir" ]; then
            continue
        fi
        name="${dir##*/}"
        case "$name" in
            current | source | "$version" | "$previous") continue ;;
        esac
        rm -rf "$dir"
        say "Removed the old version $dir."
    done
}

link_binaries() {
    mkdir -p "$bin_dir"
    linked=""
    for b in $BINARIES; do
        link="$bin_dir/$b"
        target="$app_root/current/bin/$b"
        if [ -L "$link" ]; then
            case "$(readlink "$link")" in
                "$app_root"/*) ;;
                *)
                    say "$link links somewhere else, so the script leaves it alone."
                    continue
                    ;;
            esac
        elif [ -e "$link" ]; then
            say "$link exists and is not a link from this script, so the script leaves it alone."
            continue
        fi
        ln -sfn "$target" "$link"
        linked="${linked:+$linked }$b"
    done
    if [ -n "$linked" ]; then
        say "Linked $linked into $bin_dir."
    fi
}

# Prints the program path for the Exec key of a desktop entry, quoted when needed.
desktop_exec_path() {
    case "$1" in
        *[!A-Za-z0-9/._+-]*) printf '"%s"' "$(printf '%s' "$1" | sed 's/%/%%/g')" ;;
        *) printf '%s' "$1" ;;
    esac
}

install_menu_entry() {
    icon_sizes=""
    for png in "$app_dir"/share/icons/hicolor/*/apps/meerora.png; do
        [ -f "$png" ] || continue
        size="${png%/apps/meerora.png}"
        size="${size##*/}"
        mkdir -p "$icon_root/$size/apps"
        cp "$png" "$icon_root/$size/apps/meerora.png"
        icon_sizes="${icon_sizes:+$icon_sizes }$size"
    done
    say "Installed the icon to $icon_root."
    desktop_ours=0
    if [ -e "$desktop_file" ] && ! grep -qF "$DESKTOP_MARK" "$desktop_file"; then
        say "$desktop_file exists and was not written by this script, so the script leaves it alone."
        return 0
    fi
    exe="$app_root/current/bin/meerora"
    mkdir -p "${desktop_file%/*}"
    cat >"$desktop_file.new" <<EOF
[Desktop Entry]
$DESKTOP_MARK
Type=Application
Name=meerora
Comment=LLM agents as figures in one shared space
Exec=$(desktop_exec_path "$exe") --windowed
TryExec=$exe
Icon=meerora
Terminal=false
Categories=Utility;
StartupWMClass=meerora
EOF
    mv -f "$desktop_file.new" "$desktop_file"
    desktop_ours=1
    say "Wrote the menu entry $desktop_file."
}

init_data_dir() {
    if [ -e "$data_dir" ] || [ -L "$data_dir" ]; then
        say "Your data dir $data_dir exists, so the script leaves it alone."
        return 0
    fi
    say "Creating your data dir $data_dir with meerora-agentd init."
    if "$app_dir/bin/meerora-agentd" init >"$work/init.log" 2>&1; then
        sed -e '/^$/d' -e 's/^/  /' "$work/init.log"
    else
        sed 's/^/  /' "$work/init.log"
        say "meerora-agentd init failed. meerora shows its demo agents until you run meerora-agentd init yourself."
    fi
}

launch() {
    if [ "${MEERORA_NO_LAUNCH:-}" = 1 ]; then
        say "MEERORA_NO_LAUNCH=1 is set, so meerora does not start now."
    elif [ "$session" = none ]; then
        say "There is no graphical session here, so meerora does not start now."
    elif [ -n "$required" ]; then
        say "Packages are missing (see below), so meerora does not start now."
    elif has pgrep && pgrep -u "$(id -u)" -x meerora >/dev/null 2>&1; then
        say "meerora is already running. Quit it and start it again to use meerora $version."
    else
        mkdir -p "$log_dir"
        log="$log_dir/meerora.log"
        if has setsid; then
            setsid nohup "$app_dir/bin/meerora" --windowed </dev/null >"$log" 2>&1 &
        else
            nohup "$app_dir/bin/meerora" --windowed </dev/null >"$log" 2>&1 &
        fi
        pid=$!
        sleep 2
        # setsid forks when it would lead a process group, so look for the name too.
        if kill -0 "$pid" 2>/dev/null || { has pgrep && pgrep -u "$(id -u)" -x meerora >/dev/null 2>&1; }; then
            say "meerora started in a window. Its output goes to $log."
        else
            say "meerora quit right after it started. These are the last lines of $log:"
            tail -n 20 "$log" | sed 's/^/  /'
        fi
    fi
}

# Writes the uninstall steps to UNINSTALL.txt in the app folder and prints them.
uninstall_notes() {
    {
        say "To uninstall meerora, remove what the install script created:"
        say "  rm -rf $(shell_quote "$app_root")"
        for b in $linked; do
            say "  rm -f $(shell_quote "$bin_dir/$b")"
        done
        if [ "$desktop_ours" = 1 ]; then
            say "  rm -f $(shell_quote "$desktop_file")"
        fi
        for size in $icon_sizes; do
            say "  rm -f $(shell_quote "$icon_root/$size/apps/meerora.png")"
        done
        say "Your data dir $data_dir holds your agents, rooms and settings, and"
        say "$log_dir holds the log. Both stay unless you remove them yourself."
    } >"$app_root/UNINSTALL.txt"
    cat "$app_root/UNINSTALL.txt"
}

summary() {
    step "Done"
    say "meerora $version is installed in $app_dir."
    case ":$PATH:" in
        *":$bin_dir:"*) say "Start it from your app menu or with meerora --windowed." ;;
        *)
            say "Start it from your app menu or with $bin_dir/meerora --windowed."
            say "$bin_dir is not on your PATH in this shell. Most systems add it at your next login."
            ;;
    esac
    if [ -n "$required" ]; then
        say ""
        say "Install these packages before you start meerora:"
        say "  $(install_command "$required")"
    fi
    if [ -n "$recommended" ]; then
        say ""
        say "This package is recommended:"
        say "  $(install_command "$recommended")"
    fi
    say ""
    say "Claude and Rivet, your two starting agents, run on the claude CLI"
    say "(https://claude.com/claude-code). It must be installed and logged in, so run"
    say "claude once and log in before you talk to them. The Codex CLI or a local"
    say "Ollama model work too, see $REPO_URL#readme."
    if has claude; then
        say "The claude CLI is at $(command -v claude)."
    else
        say "The claude CLI is not on your PATH yet."
    fi
    say "meerora-agentd check tells whether the agent sandbox works."
    say ""
    uninstall_notes
}

cleanup() {
    if [ -n "${work:-}" ] && [ -d "$work" ]; then
        rm -rf "$work"
    fi
}

main() {
    from_source=0
    for arg in "$@"; do
        case "$arg" in
            --from-source) from_source=1 ;;
            -h | --help)
                usage
                exit 0
                ;;
            *) fail "Unknown option $arg. Run it with --help to see the options." ;;
        esac
    done

    check_platform
    [ -n "${HOME:-}" ] || fail "HOME is not set."
    if [ "$(id -u)" -eq 0 ]; then
        fail "Run it as your own user, without sudo. It installs meerora into your home folder."
    fi

    data_home="${XDG_DATA_HOME:-$HOME/.local/share}"
    app_root="$data_home/meerora-app"
    bin_dir="$HOME/.local/bin"
    icon_root="$data_home/icons/hicolor"
    desktop_file="$data_home/applications/meerora.desktop"
    data_dir="${MEERORA_DATA_DIR:-$data_home/meerora}"
    log_dir="${XDG_STATE_HOME:-$HOME/.local/state}/meerora"
    required=""
    recommended=""
    tools=""
    linked=""
    icon_sizes=""
    desktop_ours=0
    work=""
    trap cleanup EXIT
    trap 'exit 1' INT TERM HUP

    user_name="$(id -un 2>/dev/null)" || user_name="$(id -u)"
    say "This installs meerora for $user_name into $app_root."
    detect_package_manager

    step "Checking this system"
    check_system

    step "Getting meerora"
    prepare_app_root
    if [ -n "${MEERORA_TARBALL:-}" ]; then
        use_given_tarball
    elif [ "$from_source" = 1 ]; then
        build_from_source
    else
        download_release
    fi
    verify_checksum
    unpack

    step "Checking the libraries"
    check_libraries "$new_dir"

    step "Installing"
    install_version
    link_binaries
    install_menu_entry
    init_data_dir

    step "Starting meerora"
    launch

    summary
}

main "$@"
