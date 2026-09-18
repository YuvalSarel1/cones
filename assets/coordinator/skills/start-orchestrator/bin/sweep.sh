#!/bin/bash
# Roster sweep: who is running in $WB right now, as cones reports it. Prints "same", or "changed"
# followed by the sections that moved. `cones ls --dir` is the discovery source; see fleet.py for
# what that decides and why this no longer reads the registries or the process table itself.
# mail: lines are new entries of the folder's inbox, ~/.claude/orchestrator/<sha1 of WB>/inbox.jsonl,
# one JSON line per message from any agent that cannot SendMessage (Codex, another harness):
# {"from":"codex:<thread>","text":"..."}. Read position lives in $D/inbox.pos.
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
INBOX=~/.claude/orchestrator/$(printf '%s' "$WB" | shasum -a 1 | cut -c1-40)/inbox.jsonl
mkdir -p "$(dirname "$INBOX")"; touch "$INBOX"
have=$(wc -l < "$INBOX" | tr -d ' '); seen=$(cat "$D/inbox.pos" 2>/dev/null); seen=${seen:-0}
mail=""; [ "$have" -gt "$seen" ] && mail=$(tail -n +"$((seen+1))" "$INBOX")
if [ -z "$delta" ] && [ -z "$mail" ] && [ "$delivery_changed" = 0 ] && [ "$roster_changed" = 0 ]; then
  echo same
else
  echo changed
  if [ "$delivery_changed" = 1 ]; then printf 'delivery: %s\n' "${delivery_error:-recovered}"; fi
  if [ "$roster_changed" = 1 ]; then printf 'roster: %s\n' "${roster_error:-recovered}"; fi
  [ -n "$delta" ] && printf '%s\n' "$delta"
  if [ -n "$mail" ]; then echo "mail:"; printf '%s\n' "$mail"; echo "$have" > "$D/inbox.pos"; fi
  cp "$D/roster.now" "$D/roster.prev"
fi
