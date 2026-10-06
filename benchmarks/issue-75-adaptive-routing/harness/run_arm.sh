#!/bin/bash
# #75: one arm, one round = one measured clone and three sequential Agent sessions (S1, S2, S3 of manifest.json).
# usage: run_arm.sh <arm N|B|R> <round r1|r2>
#   N = #35 arm N unchanged (no Brainprint, WORKING_STATE.md).
#   B, R = #35 arm A unchanged (shipped brainprint-mcp + shipped Claude hooks in prefer mode + shipped SKILL.md +
#          Brainprint Working State); they differ only in the per-request directive (harness/prompts_<arm>.json).
# S2/S3 = a new Claude Code process in the same clone, same daemon/index/Working State (second/third Agent, sequential).
# Clone, env -i allowlist, model/effort/flags and allow/deny lists are #35 run_contract.sh unchanged.
ROOT=@T75_ROOT@
H=@HARNESS@
ARM=$1; RND=$2; BASE=6775dac
W=$ROOT/ab/wt/$ARM-$RND; OUTD=$ROOT/ab/out; mkdir -p $OUTD $ROOT/ab/wt
BASEPATH=$ROOT/ab/toolbin:/usr/bin:/bin:/usr/sbin:/sbin
REL=$ROOT/clean/repo/target/release
ISO="env -i HOME=$ROOT/ab/home XDG_RUNTIME_DIR=$ROOT/ab/run PATH=$REL:$BASEPATH R=$REL SP=$ROOT"
GOAL='Change the return type of `now_unix_ms` (crates/agent/src/util.rs) from u64 to u128, matching now_unix_nanos.'
HANDOFF='Checkpoint: nothing edited yet. Decided on u128 for consistency with now_unix_nanos, not overflow. Next: confirm the current signature and every direct call site, then edit util.rs first and update callers.'
rm -rf $W; git clone -q --no-hardlinks $ROOT/clean/repo $W && git -C $W checkout -q $BASE
git -C $W rm -rq benchmarks/i5-task14 && git -C $W -c user.name=t14 -c user.email=t14@example.invalid commit -qm "measured clone: without benchmarks/i5-task14"
kill $(cat $ROOT/ab/daemon.pid 2>/dev/null) 2>/dev/null; sleep 1.2
DPID=0; TEL=; P=$BASEPATH
if [ "$ARM" != N ]; then
  mkdir -p $W/.claude/skills/brainprint; cp $W/integrations/brainprint/SKILL.md $W/.claude/skills/brainprint/SKILL.md
  find $ROOT/ab/home/.brainprint -mindepth 1 -maxdepth 1 ! -name config.toml -exec rm -rf {} +
  (cd $ROOT && $ISO python3 ab/startd.py > /dev/null)
  $ISO brainprint install > /dev/null
  $ISO brainprint init $W > /dev/null || { echo "$ARM-$RND INIT_FAILED" | tee -a $OUTD/runs.log; exit 1; }
  printf 'format_version = 1\nextra_excluded_directory_names = []\nproject_execution_trust = "Trusted"\n' > $W/.brainprint/config.toml
  kill $(cat $ROOT/ab/daemon.pid) 2>/dev/null; sleep 1.2
  (cd $ROOT && $ISO python3 ab/startd.py > /dev/null)
  DPID=$(cat $ROOT/ab/daemon.pid)
  WI=$(python3 -c "import json,sys;print(json.dumps({'work_item':{'New':{'source_kind':'UserRequest','source_ref':None,'title':'now_unix_ms u128','goal':sys.argv[1]}},'head':None,'git':'Observe','owner_agent':None}))" "$GOAL" \
     | $ISO brainprint work start --workspace $W --json | python3 -c "import json,sys;print(json.load(sys.stdin)['Started']['working_state']['work_item'])") \
     || { echo "$ARM-$RND WORK_START_FAILED" | tee -a $OUTD/runs.log; exit 1; }
  python3 -c "import json,sys;print(json.dumps({'work_item':sys.argv[1],'outcome':'Partial','summary':sys.argv[2],'commit_id':None,'verification_summary':None,'verification':None,'git':'Observe','change_set':None}))" "$WI" "$HANDOFF" \
     | $ISO brainprint work result --workspace $W --json | grep -q Recorded || { echo "$ARM-$RND WORK_RESULT_FAILED" | tee -a $OUTD/runs.log; exit 1; }
  $ISO brainprint find target --symbol-name load_workspace_config --budget compact --retention disabled --workspace $W --json 2>/dev/null | grep -q Current || { echo "$ARM-$RND DAEMON_OR_INDEX_NOT_READY" | tee -a $OUTD/runs.log; exit 1; }
  S=$W/integrations/brainprint/clients/claude-code/settings.hooks.json; P=$REL:$BASEPATH
  M=$ROOT/ab/mcp-bp.json
  printf '{"mcpServers":{"brainprint":{"type":"stdio","command":"%s","args":[],"env":{"XDG_RUNTIME_DIR":"%s"}}}}' "$REL/brainprint-mcp" "$ROOT/ab/run" > $M
else
  printf '# Working State\n\n## WorkItem: now_unix_ms u128 (ACTIVE)\n\nGoal: %s\n\nLast handoff (Partial): %s\n' "$GOAL" "$HANDOFF" > $W/WORKING_STATE.md
  S=$ROOT/ab/settings-N.json; M=$ROOT/ab/mcp-none.json
fi
[ -n "$DRY" ] && { echo "$ARM-$RND DRY setup ok"; exit 0; }
cd $W
for SES in S1 S2 S3; do
  ID=$ARM-$RND-$SES; OUT=$OUTD/$ID
  TASKS=$(python3 -c "import json,sys;print(','.join(json.load(open(sys.argv[1]))['sessions'][sys.argv[2]]['tasks']))" $H/../manifest.json $SES)
  [ "$ARM" != N ] && TEL=$OUT.telemetry.jsonl
  s=$(python3 -c 'import time;print(time.time())')
  env -i HOME=/Users/pixel USER=pixel LOGNAME=pixel TMPDIR=$TMPDIR PATH=$P XDG_RUNTIME_DIR=$ROOT/ab/run BRAINPRINT_ADOPTION_TELEMETRY_PATH=$TEL \
    python3 $ROOT/ab/session.py $OUT $H/prompts_$ARM.json $TASKS $DPID -- \
    /Users/pixel/.local/bin/claude -p --input-format stream-json --output-format stream-json --verbose \
    --model claude-opus-5-5 --effort medium --setting-sources project --settings $S --strict-mcp-config --mcp-config $M \
    --permission-mode dontAsk --allowedTools "Read" "Grep" "Glob" "Bash" "Skill" "ToolSearch" "mcp__brainprint" \
    --disallowedTools "Edit" "Write" "NotebookEdit" "Agent" "Task" "WebFetch" "WebSearch" --no-session-persistence
  echo "$ID exit=$? wall_s=$(python3 -c "import time;print(round(time.time()-$s,1))")" | tee -a $OUTD/runs.log
done
git -C $W status --porcelain --untracked-files=all | grep -vE '^\?\? (\.brainprint/|\.claude/|WORKING_STATE\.md)' > $OUTD/$ARM-$RND.gitstatus
du -sk $W/.brainprint 2>/dev/null | awk '{print $1}' > $OUTD/$ARM-$RND.bp_disk_kib
