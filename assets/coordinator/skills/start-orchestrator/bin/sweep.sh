#!/bin/bash
# Roster sweep: who is running in $WB right now. Prints "same" or "changed" + new/gone lines.
# usage: sweep.sh D WB SELF [JOBID]
D="$1"; WB="$2"; SELF="$3"; JOB="$4"
for f in ~/.claude/sessions/*.json; do
  p=$(basename "$f" .json); kill -0 "$p" 2>/dev/null || continue
  python3 -c "import json;d=json.load(open('$f'));print(f\"{d.get('cwd','')}\t{d.get('kind','')}\t{d.get('pid','')}\t{d.get('name','')}\")" 2>/dev/null
done | awk -F'\t' -v wb="$WB" -v self="$SELF" '($1==wb || index($1, wb"/")==1) && $2=="bg" && $3!=self {print $3 "\t" $4}' | sort > "$D/roster.now"
# codex has no registry: pid + cwd
for p in $(pgrep -x codex); do
  c=$(lsof -a -p "$p" -d cwd -Fn 2>/dev/null | sed -n 's/^n//p')
  case "$c" in "$WB"|"$WB"/*) echo "$p	CODEX:$c";; esac
done >> "$D/roster.now"
python3 "$(dirname "$0")/status.py" "$D" "$WB" "$SELF" "$JOB" >/dev/null
touch "$D/roster.prev"
if cmp -s <(cut -f1 "$D/roster.now" | sort) <(cut -f1 "$D/roster.prev" | sort); then echo same; else
  echo changed; echo "new:"; comm -23 <(cut -f1 "$D/roster.now"|sort) <(cut -f1 "$D/roster.prev"|sort) | while read p; do grep "^$p	" "$D/roster.now"; done
  echo "gone:"; comm -13 <(cut -f1 "$D/roster.now"|sort) <(cut -f1 "$D/roster.prev"|sort) | while read p; do grep "^$p	" "$D/roster.prev"; done
  cp "$D/roster.now" "$D/roster.prev"; fi
