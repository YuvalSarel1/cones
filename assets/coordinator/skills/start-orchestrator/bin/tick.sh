#!/bin/bash
# One call per wake: everything the orchestrator reads before acting, in one tool result.
# First line is `quiet` when nothing moved since the last tick (tree, HEAD, roster, inbox, held),
# else `changed`. usage: tick.sh WB   (finds SELF/JOB/D itself via self.sh)
WB="$1"; S="$(dirname "$0")"
eval "$(bash "$S/self.sh" "$WB")"
cd "$WB" || exit 1
head=$(git rev-parse --short HEAD 2>/dev/null); tree=$(git status --short 2>/dev/null)
roster=$(cat "$D/roster.now" 2>/dev/null)
INBOX=~/.claude/orchestrator/$(printf '%s' "$WB" | shasum -a 1 | cut -c1-40)/inbox.jsonl
have=$(wc -l < "$INBOX" 2>/dev/null | tr -d ' '); seen=$(cat "$D/inbox.pos" 2>/dev/null); unread=$(( ${have:-0} - ${seen:-0} ))
held=$(cat "$D/held.json" 2>/dev/null)
builds=$(pgrep -fl 'cargo|rustc|npm run|tsc|vite' 2>/dev/null | grep -v pgrep | wc -l | tr -d ' ')
sig=$(printf '%s\n%s\n%s\n%s\n%s' "$head" "$tree" "$roster" "$unread" "$held" | shasum | cut -c1-12)
prev=$(cat "$D/tick.prev" 2>/dev/null); echo "$sig" > "$D/tick.prev"
[ "$sig" = "$prev" ] && echo quiet || echo changed
echo "self=$SELF job=$JOB head=$head unread_mail=$unread builds_running=$builds load=$(sysctl -n vm.loadavg 2>/dev/null | tr -d '{}' | awk '{print $1}')"
echo "--- tree"; printf '%s\n' "${tree:-clean}"
echo "--- roster (pid<TAB>name)"; printf '%s\n' "${roster:-empty}"
echo "--- held"; printf '%s\n' "${held:-[]}"
echo "--- last event"; tail -n 1 "$D/event.txt" 2>/dev/null || echo none
