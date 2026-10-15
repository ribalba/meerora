#!/bin/sh
# Builds a data dir for snapshots of the Changes and the Activity app of the
# agent view: the agents and the space from assets/, Wren as a real agent
# with a schedule and a budget, its directory as a git repository with a few
# dated runs and a Keep from the editor, logs/runs.jsonl, state.json, tile
# layouts and two events files (Wren resting, and Wren in a scheduled run).
#
#   assets/scenarios/apps-fixture.sh /tmp/apps
#   target/debug/meerora snapshot changes.png --data-dir /tmp/apps \
#       --events /tmp/apps/changes.jsonl --view agent:wren --tab changes
#   cp /tmp/apps/layouts-activity.json /tmp/apps/layouts.json
#   target/debug/meerora snapshot activity.png --data-dir /tmp/apps \
#       --events /tmp/apps/activity.jsonl --view agent:wren --tab activity
#
# The target directory must not exist yet.
set -eu

out=${1:?usage: apps-fixture.sh DIR}
[ -e "$out" ] && { echo "$out exists already" >&2; exit 1; }
here=$(cd "$(dirname "$0")/.." && pwd)
mkdir -p "$out"
cp -r "$here/agents" "$here/spaces" "$out/"
out=$(cd "$out" && pwd)
wren="$out/agents/wren"
now=$(date +%s)
ago() { echo $((now - $1)); }
stamp() { date -d "@$1" +"%Y-%m-%d %H:%M"; }

cat > "$wren/agent.toml" <<'EOF'
name = "Wren"
color = "#D9A441"
home = "window"
apps = ["chat", "changes", "activity"]

[runtime]
kind = "claude-cli"
model = "haiku"

[[schedule]]
every = "5m"
task = "Check the inbox. If a mail is important, switch letter on and tell me why."
station = "mailbox"
quiet_hours = "22:00-07:00"

[[schedule]]
at = "07:30"
task = "Write the morning digest of the newsletters."

[budget]
tokens_per_day = 200000

[capabilities]
granted = ["mail.read", "web"]
EOF

# Commits as the agent daemon makes them (meerora-agentd git.rs), at a given time.
g() { git -C "$wren" -c user.name=meerora -c user.email=agents@meerora.local -c commit.gpgsign=false "$@"; }
commit_at() {
  g add -A
  GIT_AUTHOR_DATE="$1 +0000" GIT_COMMITTER_DATE="$1 +0000" g commit -q -m "$2"
}

g init -q -b main
printf 'artifacts/\nlogs/\n' > "$wren/.gitignore"
mkdir -p "$wren/memory" "$wren/config"
cat > "$wren/memory/notes.md" <<'EOF'
# Wren

- Check the inbox every 5 minutes.
- Important: anything from the landlord, the bank or Jana.
EOF
cat > "$wren/memory/contacts.md" <<'EOF'
# Contacts

- Jana Novak, prefers calls on Tuesday
- Mr. Brandt, the landlord
EOF
commit_at "$(ago 11400)" "meerora init"

cat > "$wren/memory/inbox.md" <<'EOF'
# Inbox

- 09:02 Weekly Rust, newsletter
- 09:15 Cooking Club, newsletter
- 09:40 Local News, newsletter
EOF
t=$(ago 10740); commit_at "$t" "run $(stamp "$t"): Check the inbox. If a mail is important, switch letter on and tell me w…"

cat > "$wren/config/filters.toml" <<'EOF'
# Folders for newsletters, written by Wren.

[[filter]]
from = "weekly@rust.example"
folder = "Newsletters/Tech"

[[filter]]
from = "club@cooking.example"
folder = "Newsletters/Food"

[[filter]]
from = "news@local.example"
folder = "Newsletters/Local"
EOF
printf -- '- Newsletters go to folders, see config/filters.toml.\n' >> "$wren/memory/notes.md"
t=$(ago 9060); commit_at "$t" "run $(stamp "$t"): Sort the newsletters into folders and unsubscribe from the ones I never open"
# You reviewed everything up to here.
g update-ref refs/meerora/reviewed HEAD

cat > "$wren/memory/contacts.md" <<'EOF'
# Contacts

- Jana Novak, prefers calls on Thursday afternoon
- Tom, Jana's assistant, books her calls
- Mr. Brandt, the landlord
EOF
mkdir -p "$wren/memory/drafts"
cat > "$wren/memory/drafts/jana.md" <<'EOF'
Hi Jana,

thank you for the invitation. Tuesday does not work for me after all.
Could we move the call to Thursday afternoon, say 15:00?
Tom is welcome to send a new invitation.

Best regards
Robin
EOF
t=$(ago 3540); commit_at "$t" "run $(stamp "$t"): Draft a polite reply to Jana about moving the call to Thursday"

cat > "$wren/memory/inbox.md" <<'EOF'
# Inbox

- 09:02 Weekly Rust, newsletter, filed
- 09:15 Cooking Club, newsletter, filed
- 09:40 Local News, newsletter, filed
- 13:12 Mr. Brandt: the heating is serviced on Friday, important
EOF
t=$(ago 1440); commit_at "$t" "run $(stamp "$t"): Check the inbox. If a mail is important, switch letter on and tell me w…"

# Keep in the editor: Wren's change to the inbox joins the review (Q2), as
# the editor's set_reviewed does it, with a private index.
reviewed=$(g rev-parse refs/meerora/reviewed)
index="$out/.keep-index"
blob=$(g hash-object -w memory/inbox.md)
GIT_INDEX_FILE="$index" g read-tree "$reviewed"
GIT_INDEX_FILE="$index" g update-index --add --cacheinfo "100644,$blob,memory/inbox.md"
tree=$(GIT_INDEX_FILE="$index" g write-tree)
rm -f "$index"
t=$(ago 600)
keep=$(GIT_AUTHOR_DATE="$t +0000" GIT_COMMITTER_DATE="$t +0000" \
  git -C "$wren" -c user.name=meerora -c user.email=review@meerora.local commit-tree "$tree" -p "$reviewed" \
  -m "meerora: keep Wren's change to memory/inbox.md")
g update-ref refs/meerora/reviewed "$keep"

# The run history (logs/runs.jsonl) and the daemon's state.json.
mkdir -p "$wren/logs"
run() { # seconds ago, duration in seconds, trigger, task, outcome, summary, tokens
  start=$(( ($(ago "$1")) * 1000 ))
  end=$(( start + $2 * 1000 ))
  printf '{"id":"wren-%s","trigger":"%s","task":"%s","start":%s,"end":%s,"outcome":"%s","summary":"%s","tokens":%s}\n' \
    "$start" "$3" "$4" "$start" "$end" "$5" "$6" "$7"
}
inbox="Check the inbox. If a mail is important, switch letter on and tell me why."
{
  run 10800 52 schedule "$inbox" done "Nothing important, three newsletters." 4120
  run 9310 250 user "Sort the newsletters into folders and unsubscribe from the ones I never open" needs_you \
    "Sorted 41 newsletters into three folders. Unsubscribe from six of them?" 38400
  run 7200 31 schedule "$inbox" failed "The mail server did not answer (timeout after 30 s)." 1900
  run 3900 40 user "Draft a polite reply to Jana about moving the call to Thursday" cancelled "cancelled" 2600
  run 3700 160 user "Draft a polite reply to Jana about moving the call to Thursday" needs_you \
    "The draft is in memory/drafts/jana.md." 21800
  run 1500 58 schedule "$inbox" done "One important mail from the landlord. The letter is up." 5300
  run 420 37 schedule "$inbox" done "Nothing new." 3900
} > "$wren/logs/runs.jsonl"

today=$(date +%Y-%m-%d)
morning=$(date -d "today 07:30" +%s)
[ "$morning" -gt "$now" ] && morning=$(date -d "yesterday 07:30" +%s)
cat > "$wren/state.json" <<EOF
{
  "session": "s-wren",
  "ring": "resting",
  "activity": "nothing new",
  "params": { "letter": true },
  "tokens": { "day": "$today", "used": 81920 },
  "schedule": { "every 5m": $(( ($(ago 108)) * 1000 )), "at 07:30": $((morning * 1000)) }
}
EOF

# Two layouts: the chat beside Changes, and the chat beside Activity.
layout() {
  printf '{"wren":{"columns":[{"width":0.27,"tiles":[{"app":"chat","height":1.0}]},{"width":0.73,"tiles":[{"app":"%s","height":1.0}]}],"config":["chat","changes","activity"]}}\n' "$1"
}
layout changes > "$out/layouts.json"
layout activity > "$out/layouts-activity.json"

# Events: screen 1 without Wren's lines, then Wren resting or in a scheduled run.
wake="{\"type\":\"next_wake\",\"agent\":\"wren\",\"at\":$(( ($(ago -192)) * 1000 )),\"since\":$(( ($(ago 108)) * 1000 ))}"
base() {
  grep -v '"agent":"wren"' "$here/scenarios/screen1.jsonl" | grep -v '^//'
  echo '{"type":"agent_spawned","agent":"wren","name":"Wren","system":false}'
  echo '{"type":"param","agent":"wren","name":"letter","value":true}'
  echo '{"type":"message","agent":"wren","id":"m1","role":"user","text":"Draft a polite reply to Jana about moving the call to Thursday.","done":true,"at":0}'
  echo '{"type":"message","agent":"wren","id":"m2","role":"agent","text":"The draft is in memory/drafts/jana.md. I also noted that Tom books her calls.","done":true,"at":0}'
}
{
  base
  echo '{"type":"ring","agent":"wren","state":"resting"}'
  echo '{"type":"activity","agent":"wren","text":"nothing new"}'
  echo '{"type":"go_to","agent":"wren","station":"window"}'
  echo "$wake"
} > "$out/changes.jsonl"
{
  base
  echo "{\"type\":\"run_started\",\"agent\":\"wren\",\"run\":\"wren-$(( ($(ago 72)) * 1000 ))\",\"trigger\":\"schedule\",\"task\":\"$inbox\"}"
  echo '{"type":"ring","agent":"wren","state":"working"}'
  echo '{"type":"go_to","agent":"wren","station":"mailbox"}'
  echo '{"type":"action","agent":"wren","tool":"files.read","summary":"memory/inbox.md"}'
  echo '{"type":"action","agent":"wren","tool":"mail.read","summary":"inbox, 3 new"}'
  echo '{"type":"action","agent":"wren","tool":"tool.Grep","summary":"Brandt in memory/"}'
  echo '{"type":"action","agent":"wren","tool":"browser.open","summary":"https://mail.example/inbox/4412"}'
  echo '{"type":"action","agent":"wren","tool":"editor.patch","summary":"memory/inbox.md +3 -1"}'
  echo '{"type":"action","agent":"wren","tool":"meerora.figure_set","summary":"letter = true"}'
  echo '{"type":"action","agent":"wren","tool":"meerora.set_activity","summary":"reading 3 new mails"}'
  echo '{"type":"activity","agent":"wren","text":"reading 3 new mails"}'
  echo "$wake"
} > "$out/activity.jsonl"
echo "wrote $out"
