#!/bin/bash
# #96: ONE Claude Code session (four tasks in a row) in a fresh snapshot of 6775dac without
# benchmarks/i5-task14 -- the #32 final-rerun session, tasks, order, model and flags unchanged.
# usage: run.sh <session-id> <N|H|M> <O1|O2>
#   N = native only (no Brainprint anywhere); T7 gets WORKING_STATE.md (#32 arm A).
#   M = shipped MCP + prefer hooks + SKILL.md (#32 arm B), product built from this tree.
#   H = #96 economy path: no MCP, no hooks, no skill; `brainprint` CLI on PATH and
#       integrations/brainprint/CLI.md appended to the snapshot's CLAUDE.md.
# Brainprint runs under an isolated HOME / XDG_RUNTIME_DIR: the user's daemon is never reached.
set -u
ROOT=${I96_ROOT:?}; REPO=${I96_REPO:?}; ID=$1; COND=$2; ORDER=$3; BASE=6775dac
case $ORDER in O1) TASKS=T5,T6,T1,T7;; O2) TASKS=T7,T1,T6,T5;; *) exit 2;; esac
W=$ROOT/w/$ID; OUT=$ROOT/out/$ID; BIN=$REPO/target/debug; H=$ROOT/home; RUN=$ROOT/run
mkdir -p $ROOT/out $ROOT/w $ROOT/toolbin $H $RUN
BASEPATH=/usr/bin:/bin:/usr/sbin:/sbin
ISO="env -i HOME=$H XDG_RUNTIME_DIR=$RUN BRAINPRINT_NO_AUTOSTART=1 PATH=$BIN:$BASEPATH R=$BIN SP=$ROOT RUSTUP_HOME=/Users/pixel/.rustup CARGO_HOME=/Users/pixel/.cargo"
GOAL='Change the return type of `now_unix_ms` (crates/agent/src/util.rs) from u64 to u128, matching now_unix_nanos.'
HANDOFF='Checkpoint: nothing edited yet. Decided on u128 for consistency with now_unix_nanos, not overflow. Next: confirm the current signature and every direct call site, then edit util.rs first and update callers.'

mkdir $W && git -C $REPO archive $BASE | tar -x -C $W && rm -rf $W/benchmarks/i5-task14
[ "${COND#H}" != "$COND" ] && { printf '\n' >> $W/CLAUDE.md; cat $REPO/integrations/brainprint/CLI.md >> $W/CLAUDE.md; }
git -C $W init -q && git -C $W add -A && git -C $W -c user.name=i96 -c user.email=i96@example.invalid commit -qm "measured snapshot $BASE"

$ISO brainprint daemon stop > /dev/null 2>&1; sleep 1.2 # no daemon of an earlier session serves this one
DPID=0
if [ "$COND" != N ]; then
  find $H/.brainprint -mindepth 1 -maxdepth 1 ! -name config.toml -exec rm -rf {} + 2>/dev/null
  mkdir -p $H/.brainprint
  printf 'format_version = 1\n\n[semantic_backends.rust]\nexecutable = "/Users/pixel/.cargo/bin/rust-analyzer"\n' > $H/.brainprint/config.toml
  $ISO python3 $ROOT/startd.py > /dev/null
  $ISO brainprint install > /dev/null
  $ISO brainprint init $W > /dev/null || { echo "$ID INIT_FAILED" | tee -a $ROOT/out/runs.log; exit 1; }
  printf 'format_version = 1\nextra_excluded_directory_names = []\nproject_execution_trust = "Trusted"\n' > $W/.brainprint/config.toml
  $ISO brainprint daemon stop > /dev/null; sleep 1.2 # Trusted from the first query
  $ISO python3 $ROOT/startd.py > /dev/null; DPID=$(cat $ROOT/daemon.pid)
  WI=$(python3 -c "import json,sys;print(json.dumps({'work_item':{'New':{'source_kind':'UserRequest','source_ref':None,'title':'now_unix_ms u128','goal':sys.argv[1]}},'head':None,'git':'Observe','owner_agent':None}))" "$GOAL" \
    | $ISO brainprint work start --workspace $W --json | python3 -c "import json,sys;print(json.load(sys.stdin)['Started']['working_state']['work_item'])") \
    || { echo "$ID WORK_START_FAILED" | tee -a $ROOT/out/runs.log; exit 1; }
  python3 -c "import json,sys;print(json.dumps({'work_item':sys.argv[1],'outcome':'Partial','summary':sys.argv[2],'commit_id':None,'verification_summary':None,'verification':None,'git':'Observe','change_set':None}))" "$WI" "$HANDOFF" \
    | $ISO brainprint work result --workspace $W --json | grep -q Recorded || { echo "$ID WORK_RESULT_FAILED" | tee -a $ROOT/out/runs.log; exit 1; }
  # readiness: a structural query answers before the session starts
  $ISO brainprint relations --symbol-name fingerprint --direction incoming --workspace $W --json > /dev/null 2>&1 \
    || { echo "$ID DAEMON_OR_INDEX_NOT_READY" | tee -a $ROOT/out/runs.log; exit 1; }
else
  printf '# Working State\n\n## WorkItem: now_unix_ms u128 (ACTIVE)\n\nGoal: %s\n\nLast handoff (Partial): %s\n' "$GOAL" "$HANDOFF" > $W/WORKING_STATE.md
fi

printf '{}' > $ROOT/settings-none.json; printf '{"mcpServers":{}}' > $ROOT/mcp-none.json
S=$ROOT/settings-none.json; M=$ROOT/mcp-none.json; P=$BASEPATH; TEL=; ALLOW_MCP=
case $COND in
  H|H2) # the model sees one shell command; the wrapper only pins the isolated runtime
     printf '#!/bin/sh\nexec env HOME=%s XDG_RUNTIME_DIR=%s BRAINPRINT_NO_AUTOSTART=1 %s/brainprint "$@"\n' $H $RUN $BIN > $ROOT/toolbin/brainprint
     chmod +x $ROOT/toolbin/brainprint; P=$ROOT/toolbin:$BASEPATH;;
  M) mkdir -p $W/.claude/skills/brainprint && cp $W/integrations/brainprint/SKILL.md $W/.claude/skills/brainprint/SKILL.md
     S=$W/integrations/brainprint/clients/claude-code/settings.hooks.json
     printf '{"mcpServers":{"brainprint":{"type":"stdio","command":"%s/brainprint-mcp","args":[],"env":{"XDG_RUNTIME_DIR":"%s"}}}}' $BIN $RUN > $ROOT/mcp-M.json
     M=$ROOT/mcp-M.json; P=$BIN:$BASEPATH; TEL=$OUT.telemetry.jsonl; ALLOW_MCP=mcp__brainprint;;
esac
s=$(python3 -c 'import time;print(time.time())')
cd $W && env -i HOME=/Users/pixel USER=pixel LOGNAME=pixel TMPDIR=$TMPDIR PATH=$P XDG_RUNTIME_DIR=$RUN \
  BRAINPRINT_ADOPTION_TELEMETRY_PATH=$TEL \
  python3 $ROOT/session.py $OUT $ROOT/prompts_final.json $TASKS $DPID -- \
  ${I96_CLAUDE:-/Users/pixel/.local/bin/claude} -p --input-format stream-json --output-format stream-json --verbose \
  --model claude-opus-5-5 --effort medium --setting-sources project --settings $S --strict-mcp-config --mcp-config $M \
  --permission-mode dontAsk --allowedTools "Read" "Grep" "Glob" "Bash" "Skill" "ToolSearch" $ALLOW_MCP \
  --disallowedTools "Edit" "Write" "NotebookEdit" "Agent" "Task" "WebFetch" "WebSearch" --no-session-persistence
echo "$ID $COND $ORDER exit=$? wall_s=$(python3 -c "import time;print(round(time.time()-$s,1))")" | tee -a $ROOT/out/runs.log
git -C $W status --porcelain --untracked-files=all | grep -vE '^\?\? (\.brainprint/|\.claude/|WORKING_STATE\.md)' > $OUT.gitstatus
