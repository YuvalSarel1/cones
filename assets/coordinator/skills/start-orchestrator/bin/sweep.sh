#!/bin/bash
# Roster sweep: who is running in $WB right now, as cones reports it. Prints "same", or "changed"
# followed by the sections that moved. `cones ls --dir` is the discovery source; see fleet.py for
# what that decides and why this no longer reads the registries or the process table itself.
# mail: lines are new entries of the folder's inbox, <coordinator dir>/inbox.jsonl (codex.py inbox),
# one JSON line per message from any agent that cannot SendMessage (Codex, another harness):
# {"from":"codex:<thread>","reply_to":"<request>","text":"..."}. Nothing here acknowledges mail:
# the handled position is inbox.ack next to the inbox, and only `codex.sh ack N` moves it.
# usage: sweep.sh D WB SELF [JOBID]
D="$1"; WB="$2"; SELF="$3"
S="$(dirname "$0")"
CONES="${CONES:-cones}"
# Expiry runs in the watcher, including quiet periods. Surface a failure once until it changes.
delivery_error=""
cleanup=$(python3 -B "$S/codex.py" --workspace "$WB" sweep 2>&1) || delivery_error="$cleanup"
previous_error=$(cat "$D/delivery.error" 2>/dev/null)
delivery_changed=0
if [ "$delivery_error" != "$previous_error" ]; then
  printf '%s' "$delivery_error" > "$D/delivery.error"
  delivery_changed=1
fi
# One read per pass covers the folder and the worktrees under it. A read that fails keeps the
# previous roster: reporting every worker gone would greet them all again on recovery. An install
# without `cones ls` fails here too, which is the version check — the coordinator reports it
# rather than falling back to a second, drifting implementation of discovery.
touch "$D/roster.prev"
roster_error=""
delta=""
if fleet=$("$CONES" ls --dir "$WB" --json 2>&1); then
  printf '%s\n' "$fleet" > "$D/fleet.json"
  delta=$(printf '%s\n' "$fleet" | python3 -B "$S/fleet.py" sweep "$D" "$WB" "$SELF")
else
  roster_error="cones ls --dir failed: $(printf '%s' "$fleet" | tail -n 1)"
  cp "$D/roster.prev" "$D/roster.now"
fi
previous_roster_error=$(cat "$D/roster.error" 2>/dev/null)
roster_changed=0
if [ "$roster_error" != "$previous_roster_error" ]; then
  printf '%s' "$roster_error" > "$D/roster.error"
  roster_changed=1
fi
I=$(python3 -B "$S/codex.py" --workspace "$WB" inbox) || exit 1
INBOX="$I/inbox.jsonl"; ACK="$I/inbox.ack"
mkdir -p "$I"; touch "$INBOX"
# Two positions, deliberately. `inbox.ack` is what the coordinator durably handled; only
# `codex.sh ack` moves it, and it lives beside the inbox so a new job recovers it. `inbox.shown`
# is this watcher's own note of what it already put in front of the model, so one pending batch
# does not wake it every ten seconds. Showing mail is not handling it. self.sh carries a previous
# job's position into inbox.ack once, before this watcher is armed.
# A wake needs mail worth showing, not just an inbox longer than this job's own note of it: a
# fresh job starts with no `inbox.shown`, so gating on that alone woke the coordinator on every
# pass, forever, in any folder whose inbox already had acknowledged history.
have=$(wc -l < "$INBOX" | tr -d ' ')
ack=$(cat "$ACK" 2>/dev/null); ack=${ack:-0}
shown=$(cat "$D/inbox.shown" 2>/dev/null); shown=${shown:-0}
mail=""; [ "$have" -gt "$ack" ] && mail=$(awk -v seen="$ack" 'NR>seen{print NR "\t" $0}' "$INBOX")
arrived=0; [ -n "$mail" ] && [ "$have" -gt "$shown" ] && arrived=1
# Only two things are worth a model turn: a worker that arrived and has not been greeted, and a
# worker that reached out. A departure, and a state moving between active, idle and blocked, are
# facts the coordinator reads from `tick.sh` when it is already awake; waking for them spends a
# call to learn that somebody else is still working. The roster position advances either way, so
# an ungreeted arrival is reported once rather than on every pass.
arrivals=0; printf '%s' "$delta" | grep -q '^new:' && arrivals=1
if [ "$arrivals" = 0 ] && [ "$arrived" = 0 ] && [ "$delivery_changed" = 0 ] && [ "$roster_changed" = 0 ]; then
  echo same
else
  echo changed
  if [ "$delivery_changed" = 1 ]; then printf 'delivery: %s\n' "${delivery_error:-recovered}"; fi
  if [ "$roster_changed" = 1 ]; then printf 'roster: %s\n' "${roster_error:-recovered}"; fi
  [ -n "$delta" ] && printf '%s\n' "$delta"
  if [ -n "$mail" ]; then
    echo "mail: unacknowledged, still pending after you read it. Acknowledge with codex.sh ack N."
    printf '%s\n' "$mail"
    echo "$have" > "$D/inbox.shown"
  fi
fi
# Outside the wake decision: a change the coordinator did not wake for is still consumed, so it
# cannot come back as news once something else wakes it.
cp "$D/roster.now" "$D/roster.prev"
