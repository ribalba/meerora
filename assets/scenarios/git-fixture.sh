#!/bin/sh
# Builds a data dir and a repository for snapshots of the Git app: the agents
# and the space from assets/ in DIR/data with DIR/project as Ember's workspace
# folder, DIR/project a repository with an upstream in DIR/remote.git, one
# commit to push, a staged change, a change Ember made and a new file, and an
# events file with Ember's edit in the action log.
#
#   assets/scenarios/git-fixture.sh /tmp/git-shot
#   target/debug/meerora snapshot git.png --data-dir /tmp/git-shot/data \
#       --events /tmp/git-shot/git.jsonl --view agent:ember --layout fullscreen \
#       --open git:1=/tmp/git-shot/project --tab git:1
#
# Add `--git unified` for the unified diff, or `--git staged --git-message
# "Subject\nBody"` for the commit box with a staged file. git here runs
# without your configuration. The target directory must not exist yet.
set -eu

out=${1:?usage: git-fixture.sh DIR}
[ -e "$out" ] && { echo "$out exists already" >&2; exit 1; }
here=$(cd "$(dirname "$0")/.." && pwd)
mkdir -p "$out/data"
cp -r "$here/agents" "$here/spaces" "$out/data/"
out=$(cd "$out" && pwd)
project="$out/project"
export GIT_CONFIG_GLOBAL=/dev/null GIT_CONFIG_NOSYSTEM=1
g() { git -C "$project" -c user.name="Alex Example" -c user.email=alex@example.org -c commit.gpgsign=false "$@"; }

# Ember works on the project (agent.toml `workspace`).
{ printf 'workspace = ["%s"]\n' "$project"; cat "$here/agents/ember/agent.toml"; } > "$out/data/agents/ember/agent.toml"

mkdir -p "$project/src" "$project/tests"
git init -q --bare "$out/remote.git"
g init -q -b main
cat > "$project/README.md" <<'EOF'
# uploader

Uploads finished reports to the archive.

Run `python -m uploader report.pdf`.
EOF
cat > "$project/src/upload.py" <<'EOF'
"""Uploads a report to the archive."""

import logging
import time

import requests

log = logging.getLogger("uploader")
ARCHIVE = "https://archive.example.org/api/reports"


def upload(path: str, token: str) -> str:
    with open(path, "rb") as f:
        response = requests.post(ARCHIVE, files={"report": f}, headers={"Authorization": f"Bearer {token}"})
    response.raise_for_status()
    return response.json()["id"]


def upload_all(paths: list[str], token: str) -> list[str]:
    ids = []
    for path in paths:
        ids.append(upload(path, token))
        time.sleep(0.5)
    return ids
EOF
printf 'timeout = 30\narchive = "https://archive.example.org"\n' > "$project/config.toml"
g add -A
g commit -q -m "Upload reports to the archive"
g remote add origin "$out/remote.git"
g push -q -u origin main 2>/dev/null

# A commit of yours that waits to be pushed.
printf 'timeout = 60\narchive = "https://archive.example.org"\n' > "$project/config.toml"
g commit -q -am "Wait longer for the archive"

# Staged: a note in the README.
cat > "$project/README.md" <<'EOF'
# uploader

Uploads finished reports to the archive and retries when it is busy.

Run `python -m uploader report.pdf`. Set `UPLOADER_TOKEN` first.
EOF
g add README.md

# Ember's change: retries with a backoff, and a new test.
cat > "$project/src/upload.py" <<'EOF'
"""Uploads a report to the archive."""

import logging
import time

import requests

log = logging.getLogger("uploader")
ARCHIVE = "https://archive.example.org/api/reports"
RETRIES = 3


def upload(path: str, token: str, retries: int = RETRIES) -> str:
    for attempt in range(1, retries + 1):
        with open(path, "rb") as f:
            response = requests.post(ARCHIVE, files={"report": f}, headers={"Authorization": f"Bearer {token}"}, timeout=30)
        if response.status_code != 503:
            break
        log.warning("archive busy, attempt %d of %d", attempt, retries)
        time.sleep(2 ** attempt)
    response.raise_for_status()
    return response.json()["id"]


def upload_all(paths: list[str], token: str) -> list[str]:
    return [upload(path, token) for path in paths]
EOF
cat > "$project/tests/test_upload.py" <<'EOF'
from uploader.upload import upload


def test_retries_when_busy(archive):
    archive.busy(times=2)
    assert upload("report.pdf", "t") == "r-1"
EOF

# The screen 1 state plus Ember's edits in the action log (D12).
{
    grep -v '"editor.patch"' "$here/scenarios/screen1.jsonl"
    printf '{"type":"action","agent":"ember","tool":"editor.patch","summary":"%s"}\n' "$project/src/upload.py"
    printf '{"type":"action","agent":"ember","tool":"editor.patch","summary":"%s"}\n' "$project/tests/test_upload.py"
} > "$out/git.jsonl"
echo "wrote $out"
