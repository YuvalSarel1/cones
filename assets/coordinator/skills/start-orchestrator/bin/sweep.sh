#!/bin/bash
# Roster sweep: who is running in $WB right now. Prints "same" or "changed" + new/gone/mail lines.
# mail: lines are new entries of the folder's inbox, ~/.claude/orchestrator/<sha1 of WB>/inbox.jsonl,
# one JSON line per message from any agent that cannot SendMessage (Codex, another harness):
# {"from":"codex:<thread>","text":"..."}. Read position lives in $D/inbox.pos.
# usage: sweep.sh D WB SELF [JOBID]
D="$1"; WB="$2"; SELF="$3"; JOB="$4"
# Expiry runs in the watcher, including quiet periods. Surface a failure once until it changes.
delivery_error=""
cleanup=$(python3 -B "$(dirname "$0")/codex.py" --workspace "$WB" sweep 2>&1) || delivery_error="$cleanup"
previous_error=$(cat "$D/delivery.error" 2>/dev/null)
delivery_changed=0
if [ "$delivery_error" != "$previous_error" ]; then
  printf '%s' "$delivery_error" > "$D/delivery.error"
  delivery_changed=1
fi
for f in ~/.claude/sessions/*.json; do
  p=$(basename "$f" .json); kill -0 "$p" 2>/dev/null || continue
  # Spares: an unprompted bg session registers with spare:true (authoritative) and name==jobId (bare 8-hex).
  # Greeting one turns the greeting into its first prompt and materialises a ghost job (2026-09-14). Skip them;
  # a consumed spare is renamed (old name moves to formerNames) and shows up on the next sweep. Do not test
  # the jobs/<jobId>/ dir: it can already exist for an unused spare.
  python3 -c "
import json,os;d=json.load(open('$f'));n=d.get('name','');j=d.get('jobId');c=d.get('cwd','')
spare = bool(d.get('spare')) or (bool(j) and n==j)
# A bg job's launch dir is its state.json cwd; the registry cwd moves with EnterWorktree (2026-09-14).
try: c=json.load(open(os.path.expanduser(f'~/.claude/jobs/{j}/state.json'))).get('cwd') or c
except Exception: pass
print('' if spare else f\"{c}\t{d.get('kind','')}\t{d.get('pid','')}\t{n}\")" 2>/dev/null
done | awk -F'\t' -v wb="$WB" -v self="$SELF" '($1==wb || index($1, wb"/")==1) && $2=="bg" && $3!=self {print $3 "\t" $4}' | sort > "$D/roster.now"
# codex has no registry: pid + cwd. Helper processes (queue/resume probes) live a few seconds and
# resolve to no thread; a codex pid joins the roster only once it is 30 s old, so they never flap it.
for p in $(pgrep -x codex); do
  e=$(ps -o etime= -p "$p" 2>/dev/null | tr -d ' '); [ -n "$e" ] || continue
  case "$e" in *-*|*:*:*) ;; *) IFS=: read -r m s <<<"$e"; [ $((10#$m*60+10#$s)) -ge 30 ] || continue;; esac
  # Neither viewer is an agent: `app-server` is the daemon that holds every thread in this cwd, and
  # `resume` under a cones parent is a peek the dashboard opened, which outlives the hover. Five such
  # rows flapped this roster every 30 s on 2026-09-17 and cost the coordinator a turn each time.
  a=$(ps -ww -o command= -p "$p" 2>/dev/null)
  case "$a" in *" app-server"*) continue;; esac
  case "$a" in *" resume "*) case "$(ps -o comm= -p "$(ps -o ppid= -p "$p" | tr -d ' ')" 2>/dev/null)" in
    *cones*) continue;; esac;; esac
  # A failed lsof and "not in this folder" both give an empty $c, so dropping the row on empty
  # reports a live agent gone (seen 2026-09-17: two clients vanished and returned one cycle later).
  # Carry the previous row instead; a dead pid leaves anyway, the ps check above drops it next sweep.
  c=$(lsof -a -p "$p" -d cwd -Fn 2>/dev/null | sed -n 's/^n//p')
  if [ -z "$c" ]; then grep "^$p	CODEX:" "$D/roster.prev" 2>/dev/null; continue; fi
  case "$c" in "$WB"|"$WB"/*) echo "$p	CODEX:$c";; esac
done >> "$D/roster.now"
python3 "$(dirname "$0")/status.py" "$D" "$WB" "$SELF" "$JOB" >/dev/null
touch "$D/roster.prev"
INBOX=~/.claude/orchestrator/$(printf '%s' "$WB" | shasum -a 1 | cut -c1-40)/inbox.jsonl
mkdir -p "$(dirname "$INBOX")"; touch "$INBOX"
have=$(wc -l < "$INBOX" | tr -d ' '); seen=$(cat "$D/inbox.pos" 2>/dev/null); seen=${seen:-0}
mail=""; [ "$have" -gt "$seen" ] && mail=$(tail -n +"$((seen+1))" "$INBOX")
if cmp -s <(cut -f1 "$D/roster.now" | sort) <(cut -f1 "$D/roster.prev" | sort); then roster=same; else roster=changed; fi
if [ "$roster" = same ] && [ -z "$mail" ] && [ "$delivery_changed" = 0 ]; then echo same; else
  echo changed
  if [ "$delivery_changed" = 1 ]; then printf 'delivery: %s\n' "${delivery_error:-recovered}"; fi
  echo "new:"; comm -23 <(cut -f1 "$D/roster.now"|sort) <(cut -f1 "$D/roster.prev"|sort) | while read p; do grep "^$p	" "$D/roster.now"; done
  echo "gone:"; comm -13 <(cut -f1 "$D/roster.now"|sort) <(cut -f1 "$D/roster.prev"|sort) | while read p; do grep "^$p	" "$D/roster.prev"; done
  if [ -n "$mail" ]; then echo "mail:"; printf '%s\n' "$mail"; echo "$have" > "$D/inbox.pos"; fi
  cp "$D/roster.now" "$D/roster.prev"; fi
