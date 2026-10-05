#!/bin/bash
# I5 Task 14 final rerun (base 6775dac). ONE session (one Claude Code process, four tasks in a row) in its own
# fresh clone at the base commit, benchmarks/i5-task14/ removed for A and B alike.
# usage: run_final.sh <session-id> <cond A|B> <order O1|O2>
#   A = no Brainprint (no MCP, no hooks, no skill). Native exploration only.
#   B = the shipped product path: brainprintd + brainprint-mcp + integrations/brainprint/clients/claude-code/settings.hooks.json
#       (prefer) + integrations/brainprint/SKILL.md as a project skill. No extra instruction beyond the tracked CLAUDE.md.
#   O1 = T5 T6 T1 T7     O2 = T7 T1 T6 T5
# T7 (resume) needs a previous session's Working State. B: recorded through the product `brainprint work start/result`.
# The measured Claude Code process gets `env -i` plus an allowlist: no inherited ANTHROPIC_BASE_URL (a local
# compression proxy), CLAUDECODE or parent-session variables reach it.
# A: the same goal and handoff text as WORKING_STATE.md at the clone root (A has no Working State store).
ROOT=@T14_ROOT@
ID=$1; COND=$2; ORDER=$3; BASE=6775dac
case $ORDER in O1) TASKS=T5,T6,T1,T7;; O2) TASKS=T7,T1,T6,T5;; S) TASKS=T7;; *) echo "bad order"; exit 2;; esac
W=$ROOT/ab/wtf/$ID; OUT=$ROOT/ab/out_f/$ID; mkdir -p $ROOT/ab/out_f $ROOT/ab/wtf
# toolbin = rustup proxies only: ~/.cargo/bin also holds an unrelated global brainprint install, never on PATH here
BASEPATH=@T14_ROOT@/ab/toolbin:/usr/bin:/bin:/usr/sbin:/sbin
ISO="env -i HOME=$ROOT/ab/home XDG_RUNTIME_DIR=$ROOT/ab/run PATH=$ROOT/clean/repo/target/release:$BASEPATH R=$ROOT/clean/repo/target/release SP=$ROOT RUSTUP_HOME=/Users/pixel/.rustup CARGO_HOME=/Users/pixel/.cargo"
GOAL='Change the return type of `now_unix_ms` (crates/agent/src/util.rs) from u64 to u128, matching now_unix_nanos.'
HANDOFF='Checkpoint: nothing edited yet. Decided on u128 for consistency with now_unix_nanos, not overflow. Next: confirm the current signature and every direct call site, then edit util.rs first and update callers.'
git clone -q --no-hardlinks $ROOT/clean/repo $W && git -C $W checkout -q $BASE
git -C $W rm -rq benchmarks/i5-task14 && git -C $W -c user.name=t14 -c user.email=t14@example.invalid commit -qm "measured clone: without benchmarks/i5-task14"
kill $(cat $ROOT/ab/daemon.pid 2>/dev/null) 2>/dev/null; sleep 1.2   # no daemon of an earlier session serves this one
DPID=0
if [ "$COND" = B ]; then
  mkdir -p $W/.claude/skills/brainprint && cp $W/integrations/brainprint/SKILL.md $W/.claude/skills/brainprint/SKILL.md
  # fresh registry per B session (a reused clone path with a stale registry entry makes `init` refuse): keep the global
  # config.toml (rust-analyzer locator), drop the rest of the isolated ~/.brainprint, `install` again
  find $ROOT/ab/home/.brainprint -mindepth 1 -maxdepth 1 ! -name config.toml -exec rm -rf {} +
  $ISO python3 $ROOT/ab/startd.py > /dev/null
  $ISO brainprint install > /dev/null
  $ISO brainprint init $W > /dev/null || { echo "$ID INIT_FAILED" | tee -a $ROOT/ab/out_f/runs.log; exit 1; }
  printf 'format_version = 1\nextra_excluded_directory_names = []\nproject_execution_trust = "Trusted"\n' > $W/.brainprint/config.toml
  kill $(cat $ROOT/ab/daemon.pid) 2>/dev/null; sleep 1.2   # Trusted from the first query
  $ISO python3 $ROOT/ab/startd.py > /dev/null
  DPID=$(cat $ROOT/ab/daemon.pid)
  WI=$(python3 -c "import json,sys;print(json.dumps({'work_item':{'New':{'source_kind':'UserRequest','source_ref':None,'title':'now_unix_ms u128','goal':sys.argv[1]}},'head':None,'git':'Observe','owner_agent':None}))" "$GOAL" \
     | $ISO brainprint work start --workspace $W --json | python3 -c "import json,sys;print(json.load(sys.stdin)['Started']['working_state']['work_item'])") \
     || { echo "$ID WORK_START_FAILED" | tee -a $ROOT/ab/out_f/runs.log; exit 1; }
  python3 -c "import json,sys;print(json.dumps({'work_item':sys.argv[1],'outcome':'Partial','summary':sys.argv[2],'commit_id':None,'verification_summary':None,'verification':None,'git':'Observe','change_set':None}))" "$WI" "$HANDOFF" \
     | $ISO brainprint work result --workspace $W --json | grep -q Recorded || { echo "$ID WORK_RESULT_FAILED" | tee -a $ROOT/ab/out_f/runs.log; exit 1; }
  # never measure a B session without a live daemon that answers a structural query on this Workspace
  $ISO brainprint find target --symbol-name load_workspace_config --budget compact --retention disabled --workspace $W --json 2>/dev/null | grep -q Current || { echo "$ID DAEMON_OR_INDEX_NOT_READY" | tee -a $ROOT/ab/out_f/runs.log; exit 1; }
  S=$W/integrations/brainprint/clients/claude-code/settings.hooks.json; M=$ROOT/ab/mcp-B.json; P=$ROOT/clean/repo/target/release:$BASEPATH; TEL=$OUT.telemetry.jsonl
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
echo "$ID $COND $ORDER exit=$? wall_s=$(python3 -c "import time;print(round(time.time()-$s,1))")" | tee -a $ROOT/ab/out_f/runs.log
git -C $W status --porcelain --untracked-files=all | grep -vE '^\?\? (\.brainprint/|\.claude/|WORKING_STATE\.md)' > $OUT.gitstatus
