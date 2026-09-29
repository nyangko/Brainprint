#!/bin/bash
# I5 Task 14 instruction-only experiment. One run in its own fresh worktree.
# usage: run_instr.sh <run-id> <task T1|T2> <cond A|B0|B1>
#   A  = no Brainprint (control)     B0 = Brainprint, agent picks its tools (as before)
#   B1 = B0 + the tool-selection instruction (instr.md appended to the worktree's CLAUDE.md only)
# Every worktree is a fresh clone at the base commit with benchmarks/i5-task14/ removed (A and B alike).
ROOT=@T14_ROOT@
ID=$1; T=$2; COND=$3; BASE=ed2abd0
W=$ROOT/ab/wti/$ID; OUT=$ROOT/ab/out_i/$ID
PROMPT=$(python3 -c "import json;print(json.load(open('$ROOT/ab/prompts.json'))['$T'])")
BASEPATH=/Users/pixel/.cargo/bin:/usr/bin:/bin:/usr/sbin:/sbin
ISO="env HOME=$ROOT/ab/home XDG_RUNTIME_DIR=$ROOT/ab/run PATH=$ROOT/clean/repo/target/release:$BASEPATH R=$ROOT/clean/repo/target/release SP=$ROOT RUSTUP_HOME=/Users/pixel/.rustup CARGO_HOME=/Users/pixel/.cargo"
git clone -q --no-hardlinks $ROOT/clean/repo $W && git -C $W checkout -q $BASE
git -C $W rm -rq benchmarks/i5-task14 && git -C $W -c user.name=t14 -c user.email=t14@example.invalid commit -qm "measured worktree: without benchmarks/i5-task14"
if [ "$COND" != A ]; then
  mkdir -p $W/.claude/skills/brainprint && cp $W/integrations/brainprint/SKILL.md $W/.claude/skills/brainprint/SKILL.md
  [ "$COND" = B1 ] && cat $ROOT/ab/instr.md >> $W/CLAUDE.md
  $ISO brainprint init $W > /dev/null
  printf 'format_version = 1\nextra_excluded_directory_names = []\nproject_execution_trust = "Trusted"\n' > $W/.brainprint/config.toml
  # the daemon reads the Workspace config when it first opens it: restart so it is Trusted from the first query
  kill $(cat $ROOT/ab/daemon.pid) 2>/dev/null; sleep 1.2
  $ISO python3 $ROOT/ab/startd.py > /dev/null
  # never measure a B run without a live daemon that already answers a structural query on this Workspace
  $ISO brainprint find target --symbol-name load_workspace_config --budget compact --retention disabled --workspace $W --json 2>/dev/null | grep -q Current || { echo "$ID DAEMON_OR_INDEX_NOT_READY" | tee -a $ROOT/ab/out_i/runs.log; exit 1; }
  S=$ROOT/ab/settings-B-prefer.json; M=$ROOT/ab/mcp-B.json; P=$ROOT/clean/repo/target/release:$BASEPATH; TEL=$OUT.telemetry.jsonl
else S=$ROOT/ab/settings-A.json; M=$ROOT/ab/mcp-A.json; P=$BASEPATH; TEL=; fi
cd $W
s=$(python3 -c 'import time;print(time.time())')
env HOME=/Users/pixel PATH=$P XDG_RUNTIME_DIR=$ROOT/ab/run BRAINPRINT_ADOPTION_TELEMETRY_PATH=$TEL /Users/pixel/.local/bin/claude -p "$PROMPT" \
  --model claude-opus-5-5 --effort medium --setting-sources project --settings $S --strict-mcp-config --mcp-config $M \
  --permission-mode dontAsk --allowedTools "Read" "Grep" "Glob" "Bash" "Skill" "ToolSearch" "mcp__brainprint" \
  --disallowedTools "Edit" "Write" "NotebookEdit" "Agent" "Task" "WebFetch" "WebSearch" \
  --no-session-persistence --output-format stream-json --verbose < /dev/null > $OUT.stream.jsonl 2> $OUT.stderr
echo "$ID $T $COND exit=$? wall_s=$(python3 -c "import time;print(round(time.time()-$s,1))")" | tee -a $ROOT/ab/out_i/runs.log
