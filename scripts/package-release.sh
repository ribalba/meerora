#!/bin/sh
# package-release.sh builds the release binaries and packs them with the
# shipped assets into meerora-x86_64-linux.tar.gz and its .sha256.
#
#   scripts/package-release.sh                     build and pack into target/package/
#   scripts/package-release.sh --out DIR           write the tarball to DIR
#   scripts/package-release.sh --binaries DIR      pack the binaries in DIR instead of building
#   scripts/package-release.sh --version v0.1.0    the version the tarball records
#
# The release workflow (.github/workflows/release.yml) and the source build of
# website/site/install.sh both run it, so the layout is defined here once. The
# tarball unpacks to one folder that is laid out like an install prefix:
#
#   meerora-x86_64-linux/
#     VERSION                                     the version, for example v0.1.0
#     README.txt
#     bin/meerora, bin/meerora-agentd, bin/meerora-mcp
#     share/meerora/                              spaces, agents and scenarios, and licenses/
#     share/icons/hicolor/NxN/apps/meerora.png    the logo in 7 sizes
#
# The binaries find share/meerora next to their bin/ folder
# (meerora_protocol::assets_dir), the same way /usr/local/bin/meerora finds
# /usr/local/share/meerora after scripts/install-session.sh. The app starts
# meerora-agentd from its own folder, and the daemon finds meerora-mcp there.
#
# whisper.cpp is built with GGML_NATIVE=OFF unless GGML_NATIVE is set, so the
# binary runs on other CPUs than the build machine (it needs AVX2). cargo does
# not notice a change of GGML_NATIVE, so run cargo clean -p whisper-rs-sys
# --release after changing it in an existing target dir.

set -eu

repo="$(cd "$(dirname "$0")/.." && pwd)"
out="$repo/target/package"
binaries=""
version=""

NAME=meerora-x86_64-linux
BINARIES="meerora meerora-agentd meerora-mcp"
# The same folders and exclusions as scripts/install-session.sh.
ASSET_PARTS="spaces agents scenarios"
ASSET_EXCLUDES=".git logs artifacts state.json state.json.tmp grants.toml settings.toml layouts.json"
ICON_SIZES="16 32 48 64 128 256 512"

usage() {
    cat <<'EOF'
Usage: scripts/package-release.sh [options]

Options:
  --out DIR        Write the tarball and its .sha256 to DIR. Defaults to target/package.
  --binaries DIR   Pack meerora, meerora-agentd and meerora-mcp from DIR instead of building them.
  --version TEXT   The version in the VERSION file. Defaults to git describe.
  -h, --help       Show this help.
EOF
}

fail() {
    printf 'package-release.sh stopped. %s\n' "$*" >&2
    exit 1
}

while [ $# -gt 0 ]; do
    case "$1" in
        --out)
            out="${2:?--out needs a directory}"
            shift
            ;;
        --binaries)
            binaries="${2:?--binaries needs a directory}"
            shift
            ;;
        --version)
            version="${2:?--version needs a value}"
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

if [ -z "$version" ]; then
    cargo_version="$(sed -n 's/^version = "\(.*\)"$/\1/p' "$repo/Cargo.toml" | head -n 1)"
    version="$(git -C "$repo" describe --tags --dirty 2>/dev/null || true)"
    if [ -z "$version" ]; then
        commit="$(git -C "$repo" rev-parse --short HEAD 2>/dev/null || true)"
        version="v${cargo_version:-0}${commit:+-g$commit}"
    fi
fi
case "$version" in
    *[!A-Za-z0-9._+-]* | "" | .*) fail "The version $version may only contain letters, digits and . _ + - and must not start with a dot." ;;
esac

if [ -z "$binaries" ]; then
    GGML_NATIVE="${GGML_NATIVE:-OFF}"
    export GGML_NATIVE
    echo "Building the release binaries (GGML_NATIVE=$GGML_NATIVE)."
    (cd "$repo" && cargo build --release --locked -p meerora -p meerora-agentd -p meerora-mcp)
    target_dir="$(cd "$repo" && cargo metadata --no-deps --format-version 1 | grep -o '"target_directory":"[^"]*"' | cut -d'"' -f4)"
    binaries="${target_dir:-$repo/target}/release"
fi
for b in $BINARIES; do
    [ -x "$binaries/$b" ] || fail "There is no $binaries/$b."
done
for part in $ASSET_PARTS; do
    [ -d "$repo/assets/$part" ] || fail "There is no $repo/assets/$part."
done

mkdir -p "$out"
out="$(cd "$out" && pwd)"
stage="$out/$NAME"
rm -rf "$stage"
mkdir -p "$stage/bin" "$stage/share/meerora/licenses"

for b in $BINARIES; do
    cp "$binaries/$b" "$stage/bin/$b"
    chmod 0755 "$stage/bin/$b"
done

set --
for e in $ASSET_EXCLUDES; do
    set -- "$@" "--exclude=$e"
done
# shellcheck disable=SC2086 # ASSET_PARTS is a list of folder names.
tar -C "$repo/assets" "$@" -cf - $ASSET_PARTS | tar -C "$stage/share/meerora" --no-same-owner -xf -
cp "$repo/LICENSE" "$repo/assets/fonts/OFL-BricolageGrotesque.txt" "$repo/assets/fonts/OFL-IBMPlexMono.txt" \
    "$stage/share/meerora/licenses/"

for s in $ICON_SIZES; do
    mkdir -p "$stage/share/icons/hicolor/${s}x$s/apps"
    cp "$repo/assets/logo/meerora-logo-$s.png" "$stage/share/icons/hicolor/${s}x$s/apps/meerora.png"
done

printf '%s\n' "$version" >"$stage/VERSION"
cat >"$stage/README.txt" <<EOF
meerora $version for Linux on x86_64, https://github.com/ribalba/meerora

The install script https://meerora.com/install.sh unpacks this folder to
~/.local/share/meerora-app/$version and links the binaries into ~/.local/bin.
To run it from here instead, start bin/meerora --windowed. The binaries find
their shipped assets in share/meerora next to bin/.

meerora is licensed under the AGPL 3.0 or later, see share/meerora/licenses/.
EOF
chmod -R u=rwX,go=rX "$stage"

tarball="$out/$NAME.tar.gz"
tar -C "$out" --sort=name --owner=0 --group=0 --numeric-owner -czf "$tarball.new" "$NAME"
mv -f "$tarball.new" "$tarball"
(cd "$out" && sha256sum "$NAME.tar.gz" >"$NAME.tar.gz.sha256")
rm -rf "$stage"

echo "Packed meerora $version:"
echo "  $tarball"
echo "  $tarball.sha256"
