#!/bin/bash
# Two-way channel to a Codex agent on the roster (CODEX:<cwd> rows from sweep.sh).
#   codex.sh thread <pid>            -> "<thread-uuid>\t<rollout-path>"
#   codex.sh send   <pid> <message>  -> queues the message; Codex runs it when its current turn ends
#   codex.sh last   <pid>            -> last assistant text in the rollout (the reply, once it lands)
# send appends the reply protocol: one JSON line into ~/.claude/orchestrator/<sha1 of WB>/inbox.jsonl
# (WB from $WB, else $PWD); sweep.sh prints new lines as mail: and the watcher fires on them.
# Delivery needs a live client on the thread (TUI or `codex --remote ... resume`); a queued message
# to an idle thread waits in ~/.codex/queue_1.sqlite until one attaches. Gating is advisory: Codex
# has no pre-commit hook the orchestrator can hold.
# ponytail: thread resolution = `resume <uuid>` on the command line, else the thread whose title
# (first prompt) matches the prompt in argv, else newest thread in Codex's state db for the cwd.
set -e
cmd="$1"; pid="$2"; shift 2 || true
DB=$(ls -t ~/.codex/state_*.sqlite | head -1)
thread() {
  local id cwd
  # A thread id in place of a pid. Title matching is best-effort and falls through to "newest in
  # cwd", which on 2026-09-17 sent one thread's ordering to another's client; when the caller knows
  # the thread, never re-derive it from a process.
  case "$pid" in
    [0-9a-f]????????-????-????-????-???????????? | ????????-????-????-????-????????????)
      printf '%s\t%s\n' "$pid" "$(sqlite3 "$DB" "select rollout_path from threads where id='$pid'")"; return ;;
  esac
  # `resume -- <id>` puts the separator between the subcommand and the operand, so a key that
  # expects the uuid right after `resume` never matches and the pid falls through to "newest in
  # cwd" - which can name another agent's thread and send a ruling to the wrong writer.
  id=$(ps -o command= -p "$pid" | sed -n 's/.* resume \(-- \)\{0,1\}\([0-9a-f-]\{36\}\).*/\2/p')
  if [ -z "$id" ]; then
    cwd=$(lsof -a -p "$pid" -d cwd -Fn 2>/dev/null | sed -n 's/^n//p' | sed "s/'/''/g")
    # A thread's title is its first prompt, and a client started with a prompt carries it in argv
    # after `-C <cwd>`; three fresh threads in one cwd resolve by that prefix, newest as fallback.
    # argv is `... -C <cwd> -- <prompt>`; the title holds the prompt without the separator, so
    # leaving `-- ` on the key never matches and every pid falls through to the newest thread.
    # `ps` renders a newline in the prompt as the four characters \012 while the title holds a real
    # one, so a multi-line prompt never matched. Compare only the first line, and compare it to that
    # many characters of the title rather than a fixed 40, since the line can be shorter.
    prompt=$(ps -o command= -p "$pid" | sed -n 's/.* -C [^ ]* \(-- \)\{0,1\}//p' | sed 's/\\012.*//' | cut -c1-40 | sed "s/'/''/g")
    [ -n "$prompt" ] && id=$(sqlite3 "$DB" "select id from threads where cwd='$cwd' and archived=0 and substr(title,1,length('$prompt'))='$prompt' order by created_at desc limit 1")
    if [ -z "$id" ]; then
      id=$(sqlite3 "$DB" "select id from threads where cwd='$cwd' and archived=0 order by updated_at desc limit 1")
      [ -n "$id" ] && echo "codex.sh: pid $pid matched no thread title; using newest in $cwd" >&2
    fi
  fi
  [ -n "$id" ] || { echo "no codex thread for pid $pid" >&2; exit 1; }
  printf '%s\t%s\n' "$id" "$(sqlite3 "$DB" "select rollout_path from threads where id='$id'")"
}
case "$cmd" in
  thread) thread ;;
  send) id=$(thread | cut -f1)
        dir=~/.claude/orchestrator/$(printf '%s' "${WB:-$PWD}" | shasum -a 1 | cut -c1-40); inbox=$dir/inbox.jsonl
        # A queued message lands as a user turn, indistinguishable from the owner's own, so an idle
        # agent reads any note as the signal to resume and spends a turn on it. Mark the voice so it
        # is never mistaken for the owner, and flag the length: a ruling fits on a line, an
        # explanation does not and buys work nobody asked for.
        msg="$*"; [ ${#msg} -gt 400 ] && echo "codex.sh: ${#msg} chars - ruling, or explanation?" >&2
        # The reply protocol is four lines, and a thread needs them once: after its first answer the
        # agent knows the path, and repeating it triples the length of a one-line ruling.
        mkdir -p "$dir/briefed"; how=""
        if [ ! -e "$dir/briefed/$id" ]; then
          how="

How to answer: you cannot message the orchestrator; it reads a file every 10 s. Reply by appending ONE JSON line, then carry on:
printf '%s\\n' '{\"from\":\"codex:$id\",\"text\":\"<your answer, one paragraph>\"}' >> $inbox"
          : > "$dir/briefed/$id"
        fi
        codex queue --thread "$id" --message "[orchestrator, not the owner] $msg$how" ;;
  last) r=$(thread | cut -f2); grep '"payload":{"type":"message","id":"[^"]*","role":"assistant"' "$r" | tail -1 \
        | python3 -c 'import json,sys;l=sys.stdin.read();print("".join(c.get("text","") for c in json.loads(l)["payload"]["content"]) if l else "")' ;;
  *) sed -n '2,7p' "$0" >&2; exit 2 ;;
esac
