#!/bin/bash
# Print this session's pid and directories. usage: eval "$(self.sh WB)" → sets SELF, JOB, D
# (this run's working state) and I (the folder's persistent coordinator directory and inbox).
WB="$1"; SELF=""; JOB=""
H="${CLAUDE_CONFIG_DIR:-$HOME/.claude}"
# codex.py owns the folder's directory name, so a symlinked path or a custom home cannot
# leave the shell helpers and the delivery helper looking at two different inboxes.
I=$(python3 -B "$(dirname "$0")/codex.py" --workspace "$WB" inbox) || exit 1
if [ -n "$CLAUDE_JOB_DIR" ]; then
  JOB=$(basename "$CLAUDE_JOB_DIR")
  SELF=$(grep -l "\"jobId\": *\"$JOB\"" "$H"/sessions/*.json 2>/dev/null | head -1 | xargs -n1 basename 2>/dev/null | sed 's/.json//')
  D="$CLAUDE_JOB_DIR/tmp"
fi
if [ -z "$SELF" ]; then   # walk the parent chain to the first registered session (cones runner, interactive)
  p=$$; while [ "$p" -gt 1 ]; do [ -f "$H/sessions/$p.json" ] && { SELF=$p; break; }; p=$(ps -o ppid= -p "$p" | tr -d ' '); done
  D="$I"
fi
mkdir -p "$D" "$I"; touch "$I/inbox.jsonl"
# The acknowledged mail position lives beside the persistent inbox so it survives this job's
# directory. Set it once at startup: carry over a previous job's read position when there is one,
# otherwise record the existing inbox as handled and say so, rather than replaying a folder's
# whole history as new mail. After this, only `codex.sh ack` moves it.
if [ ! -f "$I/inbox.ack" ]; then
  if [ -f "$D/inbox.pos" ]; then cp "$D/inbox.pos" "$I/inbox.ack"
  else
    n=$(wc -l < "$I/inbox.jsonl" 2>/dev/null | tr -d ' '); n=${n:-0}; echo "$n" > "$I/inbox.ack"
    [ "$n" -gt 0 ] && echo "note: $n inbox entries predate acknowledgement and are not replayed" >&2
  fi
fi
echo "SELF=$SELF; JOB=$JOB; D=$D; I=$I"
