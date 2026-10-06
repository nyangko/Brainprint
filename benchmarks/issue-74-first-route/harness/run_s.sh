#!/bin/bash
# #74 Phase 2: one #32/#35 final session (one Claude Code process, four tasks in a row) for arm N or S.
# usage: run_s.sh <session-id> <arm N|S> <order O1|O2>
#   N = native only: the #35 N arm unchanged (WORKING_STATE.md, no Brainprint, no hooks, no MCP).
#   S = N + Brainprint daemon indexing the clone + ONLY the PreToolUse first-route hook (s_hook.py). No MCP
#       server, no SKILL.md, no shipped hooks, no brainprint binary on the Agent's PATH: Brainprint is not
#       model-visible. Working State as N (WORKING_STATE.md), since the Agent cannot reach Brainprint.
# Everything else (clone at 6775dac without benchmarks/i5-task14, env -i allowlist, model/effort/flags, tool
# allow/deny lists, prompts, session.py) is #35 run_contract.sh unchanged.
ROOT=@T74_ROOT@
H=@HARNESS@
ID=$1; ARM=$2; ORDER=$3; BASE=6775dac
case $ORDER in O1) TASKS=T5,T6,T1,T7;; O2) TASKS=T7,T1,T6,T5;; *) echo "bad order"; exit 2;; esac
W=$ROOT/ab/wts/$ID; OUT=$ROOT/ab/out_s/$ID; mkdir -p $ROOT/ab/out_s $ROOT/ab/wts
BASEPATH=$ROOT/ab/toolbin:/usr/bin:/bin:/usr/sbin:/sbin
REL=$ROOT/clean/repo/target/release
ISO="env -i HOME=$ROOT/ab/home XDG_RUNTIME_DIR=$ROOT/ab/run PATH=$REL:$BASEPATH R=$REL SP=$ROOT"
GOAL='Change the return type of `now_unix_ms` (crates/agent/src/util.rs) from u64 to u128, matching now_unix_nanos.'
HANDOFF='Checkpoint: nothing edited yet. Decided on u128 for consistency with now_unix_nanos, not overflow. Next: confirm the current signature and every direct call site, then edit util.rs first and update callers.'
rm -rf $W; git clone -q --no-hardlinks $ROOT/clean/repo $W && git -C $W checkout -q $BASE
git -C $W rm -rq benchmarks/i5-task14 && git -C $W -c user.name=t14 -c user.email=t14@example.invalid commit -qm "measured clone: without benchmarks/i5-task14"
kill $(cat $ROOT/ab/daemon.pid 2>/dev/null) 2>/dev/null; sleep 1.2
printf '# Working State\n\n## WorkItem: now_unix_ms u128 (ACTIVE)\n\nGoal: %s\n\nLast handoff (Partial): %s\n' "$GOAL" "$HANDOFF" > $W/WORKING_STATE.md
DPID=0; S=$ROOT/ab/settings-N.json
if [ "$ARM" = S ]; then
  find $ROOT/ab/home/.brainprint -mindepth 1 -maxdepth 1 ! -name config.toml -exec rm -rf {} +
  (cd $ROOT && $ISO python3 ab/startd.py > /dev/null)
  $ISO brainprint install > /dev/null
  $ISO brainprint init $W > /dev/null || { echo "$ID INIT_FAILED" | tee -a $ROOT/ab/out_s/runs.log; exit 1; }
  printf 'format_version = 1\nextra_excluded_directory_names = []\nproject_execution_trust = "Trusted"\n' > $W/.brainprint/config.toml
  kill $(cat $ROOT/ab/daemon.pid) 2>/dev/null; sleep 1.2
  (cd $ROOT && $ISO python3 ab/startd.py > /dev/null)
  DPID=$(cat $ROOT/ab/daemon.pid)
  $ISO brainprint find target --symbol-name load_workspace_config --budget compact --retention disabled --workspace $W --json 2>/dev/null | grep -q Current || { echo "$ID DAEMON_OR_INDEX_NOT_READY" | tee -a $ROOT/ab/out_s/runs.log; exit 1; }
  printf '#!/bin/bash\nexec %s "$@"\n' "$ISO" > $ROOT/ab/iso-bp; chmod +x $ROOT/ab/iso-bp
  S=$ROOT/ab/settings-S.json
  python3 -c "import json,sys;print(json.dumps({'hooks':{'PreToolUse':[{'matcher':'*','hooks':[{'type':'command','command':sys.argv[1]}]}]}}))" \
    "S_WS=$W S_BP='[\"$ROOT/ab/iso-bp\",\"brainprint\"]' S_RG=/Users/pixel/.local/bin/claude S_LOG=$OUT.hook.jsonl /usr/bin/python3 $H/s_hook.py" > $S
fi
cd $W
s=$(python3 -c 'import time;print(time.time())')
env -i HOME=/Users/pixel USER=pixel LOGNAME=pixel TMPDIR=$TMPDIR PATH=$BASEPATH XDG_RUNTIME_DIR=$ROOT/ab/run \
  python3 $ROOT/ab/session.py $OUT $ROOT/ab/prompts_final.json $TASKS $DPID -- \
  /Users/pixel/.local/bin/claude -p --input-format stream-json --output-format stream-json --verbose \
  --model claude-opus-5-5 --effort medium --setting-sources project --settings $S --strict-mcp-config --mcp-config $ROOT/ab/mcp-none.json \
  --permission-mode dontAsk --allowedTools "Read" "Grep" "Glob" "Bash" "Skill" "ToolSearch" "mcp__brainprint" \
  --disallowedTools "Edit" "Write" "NotebookEdit" "Agent" "Task" "WebFetch" "WebSearch" --no-session-persistence
echo "$ID $ARM $ORDER exit=$? wall_s=$(python3 -c "import time;print(round(time.time()-$s,1))")" | tee -a $ROOT/ab/out_s/runs.log
git -C $W status --porcelain --untracked-files=all | grep -vE '^\?\? (\.brainprint/|\.claude/|WORKING_STATE\.md)' > $OUT.gitstatus
