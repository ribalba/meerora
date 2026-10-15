#!/usr/bin/env bash
# Tests the login session scripts without touching the system.
#
# meerora-session runs with fake cage, meerora, systemctl, D-Bus tools and a
# fake polkit agent on PATH. The installer and the uninstaller run with fake
# binaries against a staging root (--root) in a temporary directory.
#
#   bash scripts/tests/session-test.sh
#
# cargo test runs it too (crates/meerora/tests/session_scripts.rs).

set -u

repo="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
launcher="$repo/scripts/meerora-session"
tmp="$(mktemp -d "${TMPDIR:-/tmp}/meerora-session-test.XXXXXX")"
# MEERORA_TEST_KEEP=1 keeps the temporary directory with the logs for a look.
if [ -n "${MEERORA_TEST_KEEP:-}" ]; then
    echo "The logs stay in $tmp."
else
    trap 'rm -rf "$tmp"' EXIT
fi
fake="$tmp/fakebin"
mkdir -p "$fake" "$tmp/home" "$tmp/run"
chmod 0700 "$tmp/run"

failures=0
checks=0

pass() {
    checks=$((checks + 1))
}

problem() {
    checks=$((checks + 1))
    failures=$((failures + 1))
    printf 'FAILED %s\n' "$*"
}

# expect_line FILE TEXT: FILE has a line that contains TEXT.
expect_line() {
    if grep -qF -- "$2" "$1"; then pass; else problem "$(basename "$1") should contain [$2]"; fi
}

# expect_no_line FILE TEXT: no line of FILE contains TEXT.
expect_no_line() {
    if grep -qF -- "$2" "$1"; then problem "$(basename "$1") should not contain [$2]"; else pass; fi
}

# expect_before FILE FIRST SECOND: the first line with FIRST comes before the first line with SECOND.
expect_before() {
    local a b
    a="$(grep -nF -- "$2" "$1" | head -n 1 | cut -d: -f1)"
    b="$(grep -nF -- "$3" "$1" | head -n 1 | cut -d: -f1)"
    if [ -n "$a" ] && [ -n "$b" ] && [ "$a" -lt "$b" ]; then pass; else problem "[$2] should come before [$3] in $(basename "$1")"; fi
}

expect_status() {
    if [ "$1" = "$2" ]; then pass; else problem "$3 exited with $1 instead of $2"; fi
}

fake_bin() {
    cat >"$fake/$1"
    chmod +x "$fake/$1"
}

# --- Fakes ----------------------------------------------------------------

fake_bin cage <<'EOF'
#!/bin/bash
echo "cage $*" >>"$LOG"
echo "cage sees WAYLAND_DISPLAY=${WAYLAND_DISPLAY:-} DISPLAY=${DISPLAY:-}" >>"$LOG"
while [ $# -gt 0 ] && [ "$1" != "--" ]; do shift; done
shift
export WAYLAND_DISPLAY=wayland-fake
exec "$@"
EOF

fake_bin meerora <<'EOF'
#!/bin/bash
echo "meerora $*" >>"$LOG"
echo "env XDG_CURRENT_DESKTOP=${XDG_CURRENT_DESKTOP:-} WAYLAND_DISPLAY=${WAYLAND_DISPLAY:-} XKB_DEFAULT_LAYOUT=${XKB_DEFAULT_LAYOUT:-} XKB_DEFAULT_VARIANT=${XKB_DEFAULT_VARIANT:-} XDG_SESSION_TYPE=${XDG_SESSION_TYPE:-}" >>"$LOG"
if [ -n "${FAKE_MEERORA_WAIT:-}" ]; then
    trap 'echo "meerora got TERM" >>"$LOG"; exit 143' TERM
    echo "meerora waits" >>"$LOG"
    while :; do sleep 0.05; done
fi
# Gives the helpers time to set up their traps.
sleep 0.3
exit "${FAKE_MEERORA_STATUS:-0}"
EOF

fake_bin meerora-agentd <<'EOF'
#!/bin/bash
echo "agentd $*" >>"$LOG"
if [ "$1" = init ]; then mkdir -p "$3"; fi
EOF

fake_bin systemctl <<'EOF'
#!/bin/bash
echo "systemctl $*" >>"$LOG"
case "$*" in
    *show-environment*) exit "${FAKE_SYSTEMD_STATUS:-0}" ;;
    *is-active*) exit 3 ;;
esac
exit 0
EOF

fake_bin dbus-update-activation-environment <<'EOF'
#!/bin/bash
echo "dbus-update-activation-environment $*" >>"$LOG"
EOF

fake_bin dbus-run-session <<'EOF'
#!/bin/bash
echo "dbus-run-session $*" >>"$LOG"
while [ $# -gt 0 ] && [ "$1" != "--" ]; do shift; done
shift
export DBUS_SESSION_BUS_ADDRESS=unix:path=/fake-bus
exec "$@"
EOF

fake_bin localectl <<'EOF'
#!/bin/bash
printf '   System Locale: LANG=de_DE.UTF-8\n       VC Keymap: de\n      X11 Layout: de\n       X11 Model: pc105\n     X11 Variant: nodeadkeys\n'
EOF

for helper in fake-polkit-agent fake-portal pipewire wireplumber pipewire-pulse; do
    fake_bin "$helper" <<EOF
#!/bin/bash
trap 'echo "$helper stopped" >>"\$LOG"; exit 0' TERM
echo "$helper started" >>"\$LOG"
while :; do sleep 0.05; done
EOF
done

# Runs the launcher in a clean environment. Extra VAR=value pairs come first, then -- and the arguments.
session() {
    local vars=()
    while [ $# -gt 0 ] && [ "$1" != "--" ]; do
        vars+=("$1")
        shift
    done
    shift
    env -i HOME="$tmp/home" PATH="$fake:/usr/bin:/bin" XDG_RUNTIME_DIR="$tmp/run" \
        MEERORA_APP="$fake/meerora" MEERORA_AGENTD="$fake/meerora-agentd" MEERORA_POLKIT_AGENT="$fake/fake-polkit-agent" \
        MEERORA_PORTAL="$fake/fake-portal" "${vars[@]}" bash "$launcher" "$@"
}

# --- meerora-session ------------------------------------------------------

echo "Login session with systemd and a session bus"
log="$tmp/log-session"
session LOG="$log" DBUS_SESSION_BUS_ADDRESS="unix:path=$tmp/run/bus" FAKE_MEERORA_STATUS=7 -- 2>"$tmp/err-session"
expect_status $? 7 "the session"
expect_line "$log" "agentd init --data-dir $tmp/home/.local/share/meerora"
expect_line "$log" "cage -s -- $launcher --inside-cage"
expect_line "$log" "cage sees WAYLAND_DISPLAY= DISPLAY="
expect_line "$log" "dbus-update-activation-environment --systemd WAYLAND_DISPLAY XDG_CURRENT_DESKTOP XDG_SESSION_DESKTOP XDG_SESSION_TYPE DESKTOP_SESSION QT_QPA_PLATFORM MOZ_ENABLE_WAYLAND ELECTRON_OZONE_PLATFORM_HINT _JAVA_AWT_WM_NONREPARENTING"
expect_line "$log" "systemctl --user start meerora-session.target"
expect_line "$log" "systemctl --user start pipewire.service"
expect_line "$log" "systemctl --user start wireplumber.service"
expect_line "$log" "systemctl --user start xdg-desktop-portal.service"
expect_line "$log" "fake-polkit-agent started"
expect_line "$log" "meerora --session"
expect_line "$log" "env XDG_CURRENT_DESKTOP=meerora WAYLAND_DISPLAY=wayland-fake XKB_DEFAULT_LAYOUT=de XKB_DEFAULT_VARIANT=nodeadkeys XDG_SESSION_TYPE=wayland"
expect_line "$log" "fake-polkit-agent stopped"
expect_line "$log" "systemctl --user stop meerora-session.target"
expect_line "$log" "systemctl --user unset-environment WAYLAND_DISPLAY DISPLAY XDG_CURRENT_DESKTOP XDG_SESSION_DESKTOP XDG_SESSION_TYPE DESKTOP_SESSION"
expect_before "$log" "dbus-update-activation-environment" "meerora --session"
expect_before "$log" "meerora --session" "fake-polkit-agent stopped"
expect_before "$log" "fake-polkit-agent stopped" "systemctl --user stop meerora-session.target"
expect_line "$tmp/err-session" "meerora ended with status 7, so the session ends."

echo "A second start keeps the data directory"
log="$tmp/log-again"
session LOG="$log" DBUS_SESSION_BUS_ADDRESS="unix:path=$tmp/run/bus" -- 2>/dev/null
expect_status $? 0 "the second session"
expect_no_line "$log" "agentd init"
expect_line "$log" "meerora --session"

echo "Refuses to start inside a running desktop"
# A socket path has at most 107 bytes, so a long TMPDIR skips these two tests.
if command -v python3 >/dev/null 2>&1 &&
    python3 -c 'import socket, sys; socket.socket(socket.AF_UNIX).bind(sys.argv[1])' "$tmp/run/wayland-9" 2>/dev/null; then
    log="$tmp/log-refuse"
    : >"$log"
    session LOG="$log" WAYLAND_DISPLAY=wayland-9 -- 2>"$tmp/err-refuse"
    expect_status $? 2 "the session inside a desktop"
    expect_no_line "$log" "cage"
    expect_line "$tmp/err-refuse" "meerora --windowed"

    echo "Nested in a window of the running desktop"
    log="$tmp/log-nested"
    session LOG="$log" WAYLAND_DISPLAY=wayland-9 -- --nested 2>/dev/null
    expect_status $? 0 "the nested session"
    expect_line "$log" "cage sees WAYLAND_DISPLAY=wayland-9"
    expect_line "$log" "meerora --session"
    expect_no_line "$log" "dbus-update-activation-environment"
    expect_no_line "$log" "systemctl"
    expect_no_line "$log" "fake-polkit-agent"
else
    echo "  Skipped, because python3 could not create a socket in $tmp/run."
fi

echo "Starts a session bus when there is none"
log="$tmp/log-dbus"
session LOG="$log" -- 2>/dev/null
expect_status $? 0 "the session without a bus"
expect_line "$log" "dbus-run-session -- $launcher"
expect_line "$log" "meerora --session"

echo "Starts the helpers itself without a systemd user session"
log="$tmp/log-nosystemd"
session LOG="$log" DBUS_SESSION_BUS_ADDRESS="unix:path=$tmp/run/bus" FAKE_SYSTEMD_STATUS=1 -- 2>/dev/null
expect_status $? 0 "the session without systemd"
expect_line "$log" "dbus-update-activation-environment WAYLAND_DISPLAY"
expect_no_line "$log" "dbus-update-activation-environment --systemd"
expect_no_line "$log" "systemctl --user start"
expect_line "$log" "fake-portal started"
expect_line "$log" "fake-portal stopped"
expect_line "$log" "fake-polkit-agent stopped"

echo "SIGTERM from the display manager ends meerora and cleans up"
log="$tmp/log-term"
: >"$log"
session LOG="$log" DBUS_SESSION_BUS_ADDRESS="unix:path=$tmp/run/bus" FAKE_MEERORA_WAIT=1 -- 2>/dev/null &
pid=$!
for _ in $(seq 100); do
    grep -q "meerora waits" "$log" && break
    sleep 0.05
done
kill -TERM "$pid"
wait "$pid"
status=$?
expect_line "$log" "meerora got TERM"
expect_line "$log" "fake-polkit-agent stopped"
expect_line "$log" "systemctl --user stop meerora-session.target"
expect_status "$status" 143 "the terminated session"

echo "Explains a missing cage"
log="$tmp/log-nocage"
: >"$log"
session LOG="$log" DBUS_SESSION_BUS_ADDRESS="unix:path=$tmp/run/bus" MEERORA_CAGE=no-such-cage -- 2>"$tmp/err-nocage"
expect_status $? 1 "the session without cage"
expect_line "$tmp/err-nocage" "sudo dnf install cage"
expect_no_line "$log" "meerora --session"

# --- install-session.sh and uninstall-session.sh --------------------------

installer="$repo/scripts/install-session.sh"
uninstaller="$repo/scripts/uninstall-session.sh"
bins="$tmp/bins"
mkdir -p "$bins"
for b in meerora meerora-agentd meerora-mcp; do
    printf '#!/bin/sh\necho %s\n' "$b" >"$bins/$b"
    chmod +x "$bins/$b"
done
root="$tmp/root"

# Lists every path below the root with its mode and the checksum of every file.
state() {
    (cd "$1" && find . -printf '%p %m\n' | sort && find . -type f -exec md5sum {} + | sort)
}

echo "The installer's dry run prints the plan and changes nothing"
out="$tmp/out-dry"
bash "$installer" --dry-run --root "$root" --binaries "$bins" >"$out" 2>&1
expect_status $? 0 "the dry run"
expect_line "$out" "This is a dry run. Nothing changes."
expect_line "$out" "install -m 0755 $bins/meerora $root/usr/local/bin/meerora"
expect_line "$out" "install -m 0755 $repo/scripts/meerora-session $root/usr/local/bin/meerora-session"
expect_line "$out" "would write $root/usr/share/wayland-sessions/meerora.desktop"
expect_line "$out" "Exec=/usr/local/bin/meerora-session"
expect_line "$out" "would copy spaces agents scenarios from $repo/assets"
expect_line "$out" "would write $root/usr/local/lib/systemd/user/meerora-session.target"
expect_line "$out" "would write $root/usr/local/share/xdg-desktop-portal/meerora-portals.conf"
expect_line "$out" "install -m 0644 $repo/assets/logo/meerora-logo-256.png $root/usr/local/share/icons/hicolor/256x256/apps/meerora.png"
expect_line "$out" "would write $root/usr/local/share/applications/meerora.desktop"
if [ -e "$root" ]; then problem "the dry run created $root"; else pass; fi
out="$tmp/out-dry-build"
bash "$installer" --dry-run >"$out" 2>&1
expect_status $? 0 "the dry run with a build"
expect_line "$out" "cargo build --release -p meerora -p meerora-agentd -p meerora-mcp"
expect_line "$out" "would write /usr/local/share/meerora/install-manifest.txt"

echo "The installer fills a staging root"
mkdir -p "$root/usr/local/bin"
echo "not meerora's" >"$root/usr/local/bin/keep"
out="$tmp/out-install"
bash "$installer" --root "$root" --binaries "$bins" --yes >"$out" 2>&1
expect_status $? 0 "the install"
for f in usr/local/bin/meerora usr/local/bin/meerora-agentd usr/local/bin/meerora-mcp usr/local/bin/meerora-session; do
    if [ -x "$root/$f" ]; then pass; else problem "$f should be an executable"; fi
done
for f in usr/local/share/meerora/scenarios/demo.toml usr/local/share/meerora/licenses/LICENSE \
    usr/share/wayland-sessions/meerora.desktop usr/local/lib/systemd/user/meerora-session.target \
    usr/local/share/xdg-desktop-portal/meerora-portals.conf usr/local/share/meerora/install-manifest.txt \
    usr/local/share/applications/meerora.desktop usr/local/share/icons/hicolor/16x16/apps/meerora.png \
    usr/local/share/icons/hicolor/512x512/apps/meerora.png; do
    if [ -f "$root/$f" ]; then pass; else problem "$f should exist"; fi
done
if cmp -s "$repo/assets/logo/meerora-logo-64.png" "$root/usr/local/share/icons/hicolor/64x64/apps/meerora.png"; then
    pass
else
    problem "the 64 pixel icon should equal assets/logo/meerora-logo-64.png"
fi
expect_line "$root/usr/local/share/applications/meerora.desktop" "Icon=meerora"
expect_line "$root/usr/share/wayland-sessions/meerora.desktop" "Icon=meerora"
if ls "$root"/usr/local/share/meerora/agents/*/agent.toml >/dev/null 2>&1; then pass; else problem "the agents should be installed"; fi
if ls "$root"/usr/local/share/meerora/spaces/*/space.js >/dev/null 2>&1; then pass; else problem "the spaces should be installed"; fi
if [ -z "$(find "$root/usr/local/share/meerora" -name grants.toml -o -name settings.toml -o -name layouts.json -o -name .git -o -name logs)" ]; then
    pass
else
    problem "runtime files should stay out of the shipped assets"
fi
expect_line "$root/usr/share/wayland-sessions/meerora.desktop" "Exec=/usr/local/bin/meerora-session"
expect_line "$root/usr/share/wayland-sessions/meerora.desktop" "DesktopNames=meerora"
expect_line "$root/usr/local/share/meerora/install-manifest.txt" "tree /usr/local/share/meerora"
expect_line "$root/usr/local/share/meerora/install-manifest.txt" "dir /usr/share/wayland-sessions"
expect_no_line "$root/usr/local/share/meerora/install-manifest.txt" "dir /usr/local/bin"
expect_no_line "$root/usr/local/share/meerora/install-manifest.txt" "package cage"
expect_line "$out" "With --root the script installs no packages."
if cmp -s "$launcher" "$root/usr/local/bin/meerora-session"; then pass; else problem "the installed launcher should equal scripts/meerora-session"; fi

echo "The shipped spaces are copied unchanged"
for space in "$repo"/assets/spaces/*/space.js; do
    installed="$root/usr/local/share/meerora/spaces/${space#"$repo"/assets/spaces/}"
    if cmp -s "$space" "$installed"; then pass; else problem "$installed should equal $space"; fi
done

echo "A second install changes nothing"
before="$(state "$root")"
bash "$installer" --root "$root" --binaries "$bins" --yes >"$tmp/out-install-2" 2>&1
expect_status $? 0 "the second install"
after="$(state "$root")"
if [ "$before" = "$after" ]; then pass; else problem "the second install should leave the same files"; fi

echo "The installer leaves a foreign share directory alone"
mkdir -p "$tmp/root2/usr/local/share/meerora"
bash "$installer" --root "$tmp/root2" --binaries "$bins" --yes >"$tmp/out-foreign" 2>&1
expect_status $? 1 "the install over a foreign directory"
expect_line "$tmp/out-foreign" "this installer did not make it"

echo "The uninstaller's dry run removes nothing"
before="$(state "$root")"
bash "$uninstaller" --root "$root" --dry-run >"$tmp/out-un-dry" 2>&1
expect_status $? 0 "the uninstall dry run"
expect_line "$tmp/out-un-dry" "$root/usr/local/bin/meerora-session"
expect_line "$tmp/out-un-dry" "This was a dry run, so nothing changed."
if [ "$before" = "$(state "$root")" ]; then pass; else problem "the uninstall dry run changed files"; fi

echo "The uninstaller refuses a manifest with foreign paths"
cp -a "$root" "$tmp/root3"
echo "file /etc/passwd" >>"$tmp/root3/usr/local/share/meerora/install-manifest.txt"
before="$(state "$tmp/root3")"
bash "$uninstaller" --root "$tmp/root3" --yes >"$tmp/out-un-foreign" 2>&1
expect_status $? 1 "the uninstall with a foreign path"
if [ "$before" = "$(state "$tmp/root3")" ]; then pass; else problem "the uninstaller removed files from a bad manifest"; fi

echo "The uninstaller removes exactly what the installer added"
bash "$uninstaller" --root "$root" --yes >"$tmp/out-uninstall" 2>&1
expect_status $? 0 "the uninstall"
remaining="$(cd "$root" && find . | sort | tr '\n' ' ')"
if [ "$remaining" = ". ./usr ./usr/local ./usr/local/bin ./usr/local/bin/keep " ]; then
    pass
else
    problem "after the uninstall only usr/local/bin/keep should remain, found $remaining"
fi
bash "$uninstaller" --root "$root" --yes >"$tmp/out-uninstall-2" 2>&1
expect_status $? 0 "the second uninstall"
expect_line "$tmp/out-uninstall-2" "there is nothing to remove"

# --- Syntax ---------------------------------------------------------------

echo "Syntax"
for script in "$launcher" "$installer" "$uninstaller" "${BASH_SOURCE[0]}"; do
    if bash -n "$script"; then pass; else problem "bash -n $script"; fi
done

printf '%d checks, %d failed\n' "$checks" "$failures"
[ "$failures" -eq 0 ]
