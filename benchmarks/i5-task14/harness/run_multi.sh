#!/bin/bash
# I5 Task 14 multi-task session experiment. ONE session (one Claude Code process, six tasks in a row)
# in its own fresh worktree at the base commit, with benchmarks/i5-task14/ removed for A and B alike.
# usage: run_multi.sh <session-id> <cond A|B1> <order O1|O2>
#   A  = no Brainprint (control)
#   B1 = Brainprint + the C1 tool-selection instruction (harness/instr.md appended to the worktree's CLAUDE.md;
#        shipped integrations/brainprint/SKILL.md and the repository CLAUDE.md are untouched)
#   O1 = T1 T2 T3 T4 T5 T6      O2 = T6 T5 T4 T3 T2 T1  (reversed: task order effect)      S = T3 T4 (pipeline smoke only)
ROOT=@T14_ROOT@
ID=$1; COND=$2; ORDER=$3; BASE=4b20e91
case $ORDER in O1) TASKS=T1,T2,T3,T4,T5,T6;; O2) TASKS=T6,T5,T4,T3,T2,T1;; S) TASKS=T3,T4;; *) echo "bad order"; exit 2;; esac
W=$ROOT/ab/wtm/$ID; OUT=$ROOT/ab/out_m/$ID
BASEPATH=/Users/pixel/.cargo/bin:/usr/bin:/bin:/usr/sbin:/sbin
ISO="env HOME=$ROOT/ab/home XDG_RUNTIME_DIR=$ROOT/ab/run PATH=$ROOT/clean/repo/target/release:$BASEPATH R=$ROOT/clean/repo/target/release SP=$ROOT RUSTUP_HOME=/Users/pixel/.rustup CARGO_HOME=/Users/pixel/.cargo"
git clone -q --no-hardlinks $ROOT/clean/repo $W && git -C $W checkout -q $BASE
git -C $W rm -rq benchmarks/i5-task14 && git -C $W -c user.name=t14 -c user.email=t14@example.invalid commit -qm "measured worktree: without benchmarks/i5-task14"
kill $(cat $ROOT/ab/daemon.pid 2>/dev/null) 2>/dev/null; sleep 1.2   # no daemon of an earlier session serves this one
DPID=0
if [ "$COND" != A ]; then
  mkdir -p $W/.claude/skills/brainprint && cp $W/integrations/brainprint/SKILL.md $W/.claude/skills/brainprint/SKILL.md
  [ "$COND" = B1 ] && cat $ROOT/ab/instr.md >> $W/CLAUDE.md
  $ISO python3 $ROOT/ab/startd.py > /dev/null   # `init` needs a live daemon
  $ISO brainprint init $W > /dev/null
  printf 'format_version = 1\nextra_excluded_directory_names = []\nproject_execution_trust = "Trusted"\n' > $W/.brainprint/config.toml
  # the daemon reads the Workspace config when it first opens it: restart so it is Trusted from the first query
  kill $(cat $ROOT/ab/daemon.pid) 2>/dev/null; sleep 1.2
  $ISO python3 $ROOT/ab/startd.py > /dev/null
  DPID=$(cat $ROOT/ab/daemon.pid)
  # never measure a B session without a live daemon that already answers a structural query on this Workspace
  $ISO brainprint find target --symbol-name load_workspace_config --budget compact --retention disabled --workspace $W --json 2>/dev/null | grep -q Current || { echo "$ID DAEMON_OR_INDEX_NOT_READY" | tee -a $ROOT/ab/out_m/runs.log; exit 1; }
  S=$ROOT/ab/settings-B-prefer.json; M=$ROOT/ab/mcp-B.json; P=$ROOT/clean/repo/target/release:$BASEPATH; TEL=$OUT.telemetry.jsonl
else S=$ROOT/ab/settings-A.json; M=$ROOT/ab/mcp-A.json; P=$BASEPATH; TEL=; fi
cd $W
s=$(python3 -c 'import time;print(time.time())')
env HOME=/Users/pixel PATH=$P XDG_RUNTIME_DIR=$ROOT/ab/run BRAINPRINT_ADOPTION_TELEMETRY_PATH=$TEL \
  python3 $ROOT/ab/session.py $OUT $ROOT/ab/prompts.json $TASKS $DPID -- \
  /Users/pixel/.local/bin/claude -p --input-format stream-json --output-format stream-json --verbose \
  --model claude-opus-5-5 --effort medium --setting-sources project --settings $S --strict-mcp-config --mcp-config $M \
  --permission-mode dontAsk --allowedTools "Read" "Grep" "Glob" "Bash" "Skill" "ToolSearch" "mcp__brainprint" \
  --disallowedTools "Edit" "Write" "NotebookEdit" "Agent" "Task" "WebFetch" "WebSearch" --no-session-persistence
echo "$ID $COND $ORDER exit=$? wall_s=$(python3 -c "import time;print(round(time.time()-$s,1))")" | tee -a $ROOT/ab/out_m/runs.log
