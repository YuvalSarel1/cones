#!/bin/bash
# One call per wake: everything the orchestrator reads before acting, in one tool result.
# First line is `quiet` when nothing moved since the last tick (tree, HEAD, roster, inbox, held),
# else `changed`. usage: tick.sh WB   (finds SELF/JOB/D itself via self.sh)
# Resolve the helper directory before the cd: after it, a relative $0 would point into WB.
WB="$1"; S="$(cd -- "$(dirname -- "$0")" && pwd)"
eval "$(bash "$S/self.sh" "$WB")"
[ -n "$I" ] || { echo "tick: self.sh failed; no coordinator directory resolved"; exit 1; }
cd "$WB" || exit 1
head=$(git rev-parse --short HEAD 2>/dev/null); tree=$(git status --short 2>/dev/null)
roster=$(cat "$D/roster.now" 2>/dev/null)
have=$(wc -l < "$I/inbox.jsonl" 2>/dev/null | tr -d ' ')
ack=$(cat "$I/inbox.ack" 2>/dev/null); unread=$(( ${have:-0} - ${ack:-0} ))
held=$(cat "$D/held.json" 2>/dev/null)
builds=$(pgrep -fl 'cargo|rustc|npm run|tsc|vite' 2>/dev/null | grep -v pgrep | wc -l | tr -d ' ')
sig=$(printf '%s\n%s\n%s\n%s\n%s' "$head" "$tree" "$roster" "$unread" "$held" | shasum | cut -c1-12)
prev=$(cat "$D/tick.prev" 2>/dev/null); echo "$sig" > "$D/tick.prev"
[ "$sig" = "$prev" ] && echo quiet || echo changed
echo "self=$SELF job=$JOB head=$head unread_mail=$unread builds_running=$builds load=$(sysctl -n vm.loadavg 2>/dev/null | tr -d '{}' | awk '{print $1}')"
echo "--- tree"; printf '%s\n' "${tree:-clean}"
echo "--- roster (pid<TAB>run|session<TAB>harness<TAB>id<TAB>state<TAB>folder<TAB>title)"; printf '%s\n' "${roster:-empty}"
# What a message costs the recipient. A worker near the end of its window is a handoff, not another
# note; a window the harness never reported is unknown, which is not the same as room to spare.
echo "--- budget (id  context  cost)"
python3 -B "$S/fleet.py" budget "$D" 2>/dev/null || echo "none reported"
# Pending mail is shown on every tick until it is acknowledged, which is how a restarted
# coordinator gets back the replies the previous one never handled. Printing consumes nothing.
echo "--- mail (unacknowledged; ack with codex.sh ack N)"
# A failed read is reported like a failed roster read. Silence here would read as "no worker
# has written", which is the loss this pending inbox exists to prevent.
python3 -B "$S/codex.py" --workspace "$WB" mail \
  || echo "MAIL UNREADABLE: pending replies unknown. This is not an empty inbox."
echo "--- held"; printf '%s\n' "${held:-[]}"
echo "--- last event"; tail -n 1 "$D/event.txt" 2>/dev/null || echo none
