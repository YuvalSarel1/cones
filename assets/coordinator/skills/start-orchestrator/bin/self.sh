#!/bin/bash
# Print this session's pid and ledger dir. usage: eval "$(self.sh WB)" → sets SELF, D, JOB
WB="$1"; SELF=""; JOB=""
if [ -n "$CLAUDE_JOB_DIR" ]; then
  JOB=$(basename "$CLAUDE_JOB_DIR")
  SELF=$(grep -l "\"jobId\": *\"$JOB\"" ~/.claude/sessions/*.json 2>/dev/null | head -1 | xargs -n1 basename 2>/dev/null | sed 's/.json//')
  D="$CLAUDE_JOB_DIR/tmp"
fi
if [ -z "$SELF" ]; then   # walk the parent chain to the first registered session (cones runner, interactive)
  p=$$; while [ "$p" -gt 1 ]; do [ -f ~/.claude/sessions/$p.json ] && { SELF=$p; break; }; p=$(ps -o ppid= -p "$p" | tr -d ' '); done
  D=~/.claude/orchestrator/$(printf %s "$WB" | shasum | cut -c1-40)
fi
mkdir -p "$D"
echo "SELF=$SELF; JOB=$JOB; D=$D"
