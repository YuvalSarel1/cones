#!/bin/bash
# Two-way channel to a Codex agent on the roster (CODEX:<cwd> rows from sweep.sh).
#   codex.sh thread <pid>            -> "<thread-uuid>\t<rollout-path>"
#   codex.sh send   <pid> <message>  -> queues the message; Codex runs it when its current turn ends
#   codex.sh last   <pid>            -> last assistant text in the rollout (the reply, once it lands)
# Delivery needs a live client on the thread (TUI or `codex --remote ... resume`); a queued message
# to an idle thread waits in ~/.codex/queue_1.sqlite until one attaches. Gating is advisory: Codex
# has no pre-commit hook the orchestrator can hold.
# ponytail: thread resolution = `resume <uuid>` on the command line, else newest thread in Codex's
# state db for the process cwd; a cwd with two fresh Codex threads picks the newest.
set -e
cmd="$1"; pid="$2"; shift 2 || true
DB=$(ls -t ~/.codex/state_*.sqlite | head -1)
thread() {
  local id cwd
  id=$(ps -o command= -p "$pid" | sed -n 's/.* resume \([0-9a-f-]\{36\}\).*/\1/p')
  if [ -z "$id" ]; then
    cwd=$(lsof -a -p "$pid" -d cwd -Fn 2>/dev/null | sed -n 's/^n//p')
    id=$(sqlite3 "$DB" "select id from threads where cwd='$cwd' and archived=0 order by updated_at desc limit 1")
  fi
  [ -n "$id" ] || { echo "no codex thread for pid $pid" >&2; exit 1; }
  printf '%s\t%s\n' "$id" "$(sqlite3 "$DB" "select rollout_path from threads where id='$id'")"
}
case "$cmd" in
  thread) thread ;;
  send) id=$(thread | cut -f1); codex queue --thread "$id" --message "$*" ;;
  last) r=$(thread | cut -f2); grep '"payload":{"type":"message","id":"[^"]*","role":"assistant"' "$r" | tail -1 \
        | python3 -c 'import json,sys;l=sys.stdin.read();print("".join(c.get("text","") for c in json.loads(l)["payload"]["content"]) if l else "")' ;;
  *) sed -n '2,7p' "$0" >&2; exit 2 ;;
esac
