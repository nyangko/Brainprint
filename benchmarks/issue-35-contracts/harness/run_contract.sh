#!/bin/bash
# #35 Phase 2: one #32 final session (one Claude Code process, four tasks in a row) for one integration arm.
# usage: run_contract.sh <session-id> <arm N|A|B|C|D|E> <order O1|O2>
#   N = no Brainprint (the #32 A arm: native only).
#   A = shipped path: brainprint-mcp direct + shipped hooks + shipped SKILL.md (the #32 B arm).
#   B..E = the same shipped hooks/daemon, MCP server = harness/proxy.py <arm> in front of the same brainprint-mcp,
#          SKILL.md = harness/skills/<arm>/SKILL.md when the arm renames tools (C, D, E), shipped otherwise.
# Everything else (clone at 6775dac without benchmarks/i5-task14, fresh registry, Trusted, Working State,
# env -i allowlist, model/effort/flags) is run_final.sh unchanged.
ROOT=@T14_ROOT@
H=@HARNESS@
PY=@PY@
ID=$1; ARM=$2; ORDER=$3; BASE=6775dac
case $ORDER in O1) TASKS=T5,T6,T1,T7;; O2) TASKS=T7,T1,T6,T5;; *) echo "bad order"; exit 2;; esac
W=$ROOT/ab/wtc/$ID; OUT=$ROOT/ab/out_c/$ID; mkdir -p $ROOT/ab/out_c $ROOT/ab/wtc
BASEPATH=$ROOT/ab/toolbin:/usr/bin:/bin:/usr/sbin:/sbin
REL=$ROOT/clean/repo/target/release
ISO="env -i HOME=$ROOT/ab/home XDG_RUNTIME_DIR=$ROOT/ab/run PATH=$REL:$BASEPATH R=$REL SP=$ROOT RUSTUP_HOME=/Users/pixel/.rustup CARGO_HOME=/Users/pixel/.cargo"
GOAL='Change the return type of `now_unix_ms` (crates/agent/src/util.rs) from u64 to u128, matching now_unix_nanos.'
HANDOFF='Checkpoint: nothing edited yet. Decided on u128 for consistency with now_unix_nanos, not overflow. Next: confirm the current signature and every direct call site, then edit util.rs first and update callers.'
rm -rf $W; git clone -q --no-hardlinks $ROOT/clean/repo $W && git -C $W checkout -q $BASE
git -C $W rm -rq benchmarks/i5-task14 && git -C $W -c user.name=t14 -c user.email=t14@example.invalid commit -qm "measured clone: without benchmarks/i5-task14"
kill $(cat $ROOT/ab/daemon.pid 2>/dev/null) 2>/dev/null; sleep 1.2
DPID=0
if [ "$ARM" != N ]; then
  mkdir -p $W/.claude/skills/brainprint
  if [ -f $H/skills/$ARM/SKILL.md ]; then cp $H/skills/$ARM/SKILL.md $W/.claude/skills/brainprint/SKILL.md
  else cp $W/integrations/brainprint/SKILL.md $W/.claude/skills/brainprint/SKILL.md; fi
  find $ROOT/ab/home/.brainprint -mindepth 1 -maxdepth 1 ! -name config.toml -exec rm -rf {} +
  (cd $ROOT && $ISO python3 ab/startd.py > /dev/null)
  $ISO brainprint install > /dev/null
  $ISO brainprint init $W > /dev/null || { echo "$ID INIT_FAILED" | tee -a $ROOT/ab/out_c/runs.log; exit 1; }
  printf 'format_version = 1\nextra_excluded_directory_names = []\nproject_execution_trust = "Trusted"\n' > $W/.brainprint/config.toml
  kill $(cat $ROOT/ab/daemon.pid) 2>/dev/null; sleep 1.2
  (cd $ROOT && $ISO python3 ab/startd.py > /dev/null)
  DPID=$(cat $ROOT/ab/daemon.pid)
  WI=$(python3 -c "import json,sys;print(json.dumps({'work_item':{'New':{'source_kind':'UserRequest','source_ref':None,'title':'now_unix_ms u128','goal':sys.argv[1]}},'head':None,'git':'Observe','owner_agent':None}))" "$GOAL" \
     | $ISO brainprint work start --workspace $W --json | python3 -c "import json,sys;print(json.load(sys.stdin)['Started']['working_state']['work_item'])") \
     || { echo "$ID WORK_START_FAILED" | tee -a $ROOT/ab/out_c/runs.log; exit 1; }
  python3 -c "import json,sys;print(json.dumps({'work_item':sys.argv[1],'outcome':'Partial','summary':sys.argv[2],'commit_id':None,'verification_summary':None,'verification':None,'git':'Observe','change_set':None}))" "$WI" "$HANDOFF" \
     | $ISO brainprint work result --workspace $W --json | grep -q Recorded || { echo "$ID WORK_RESULT_FAILED" | tee -a $ROOT/ab/out_c/runs.log; exit 1; }
  $ISO brainprint find target --symbol-name load_workspace_config --budget compact --retention disabled --workspace $W --json 2>/dev/null | grep -q Current || { echo "$ID DAEMON_OR_INDEX_NOT_READY" | tee -a $ROOT/ab/out_c/runs.log; exit 1; }
  S=$W/integrations/brainprint/clients/claude-code/settings.hooks.json; P=$REL:$BASEPATH; TEL=$OUT.telemetry.jsonl
  M=$ROOT/ab/mcp-arm-$ARM.json
  if [ "$ARM" = A ]; then
    printf '{"mcpServers":{"brainprint":{"type":"stdio","command":"%s","args":[],"env":{"XDG_RUNTIME_DIR":"%s"}}}}' "$REL/brainprint-mcp" "$ROOT/ab/run" > $M
  else
    printf '{"mcpServers":{"brainprint":{"type":"stdio","command":"%s","args":["%s","%s","%s"],"env":{"XDG_RUNTIME_DIR":"%s","PROXY_LOG":"%s"}}}}' "$PY" "$H/proxy.py" "$ARM" "$REL/brainprint-mcp" "$ROOT/ab/run" "$OUT.proxy.jsonl" > $M
  fi
else
  printf '# Working State\n\n## WorkItem: now_unix_ms u128 (ACTIVE)\n\nGoal: %s\n\nLast handoff (Partial): %s\n' "$GOAL" "$HANDOFF" > $W/WORKING_STATE.md
  S=$ROOT/ab/settings-A.json; M=$ROOT/ab/mcp-A.json; P=$BASEPATH; TEL=
fi
cd $W
s=$(python3 -c 'import time;print(time.time())')
env -i HOME=/Users/pixel USER=pixel LOGNAME=pixel TMPDIR=$TMPDIR PATH=$P XDG_RUNTIME_DIR=$ROOT/ab/run BRAINPRINT_ADOPTION_TELEMETRY_PATH=$TEL \
  python3 $ROOT/ab/session.py $OUT $ROOT/ab/prompts_final.json $TASKS $DPID -- \
  /Users/pixel/.local/bin/claude -p --input-format stream-json --output-format stream-json --verbose \
  --model claude-opus-5-5 --effort medium --setting-sources project --settings $S --strict-mcp-config --mcp-config $M \
  --permission-mode dontAsk --allowedTools "Read" "Grep" "Glob" "Bash" "Skill" "ToolSearch" "mcp__brainprint" \
  --disallowedTools "Edit" "Write" "NotebookEdit" "Agent" "Task" "WebFetch" "WebSearch" --no-session-persistence
echo "$ID $ARM $ORDER exit=$? wall_s=$(python3 -c "import time;print(round(time.time()-$s,1))")" | tee -a $ROOT/ab/out_c/runs.log
git -C $W status --porcelain --untracked-files=all | grep -vE '^\?\? (\.brainprint/|\.claude/|WORKING_STATE\.md)' > $OUT.gitstatus
