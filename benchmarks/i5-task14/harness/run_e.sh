#!/bin/bash
# Measurement variant of run_one.sh (I5 Task 14 cost analysis). Same claude flags, prompt and settings.
# usage: run_e.sh <out-name> <worktree> <task T1|T2> <arm A|B> <mode prefer|guard> <eager 0|1>
#   eager=1 sets ENABLE_TOOL_SEARCH=false (MCP tool schemas are in the request from turn 0, not loaded by ToolSearch)
#   The worktree can be reused across calls: a second run in the same worktree can read the first run's prompt cache.
ROOT=@T14_ROOT@
O=$1; W=$ROOT/ab/wt/$2; T=$3; ARM=$4; MODE=$5; EAGER=$6
OUT=$ROOT/ab/out_e/$O
PROMPT=$(python3 -c "import json;print(json.load(open('$ROOT/ab/prompts.json'))['$T'])")
BASEPATH=/Users/pixel/.cargo/bin:/usr/bin:/bin:/usr/sbin:/sbin
if [ "$ARM" = A ]; then P=$BASEPATH; S=$ROOT/ab/settings-A.json; M=$ROOT/ab/mcp-A.json; TEL=
else P=$ROOT/clean/repo/target/release:$BASEPATH; S=$ROOT/ab/settings-B-$MODE.json; M=$ROOT/ab/mcp-B.json; TEL=$OUT.telemetry.jsonl; fi
EX=(); [ "$EAGER" = 1 ] && EX=(ENABLE_TOOL_SEARCH=false)
cd $W
s=$(python3 -c 'import time;print(time.time())')
env HOME=/Users/pixel "${EX[@]}" PATH=$P XDG_RUNTIME_DIR=$ROOT/ab/run BRAINPRINT_ADOPTION_TELEMETRY_PATH=$TEL /Users/pixel/.local/bin/claude -p "$PROMPT" \
  --model claude-opus-5-5 --effort medium --setting-sources project --settings $S --strict-mcp-config --mcp-config $M \
  --permission-mode dontAsk --allowedTools "Read" "Grep" "Glob" "Bash" "Skill" "ToolSearch" "mcp__brainprint" \
  --disallowedTools "Edit" "Write" "NotebookEdit" "Agent" "Task" "WebFetch" "WebSearch" \
  --no-session-persistence --output-format stream-json --verbose < /dev/null > $OUT.stream.jsonl 2> $OUT.stderr
echo "$O exit=$? wall_s=$(python3 -c "import time;print(round(time.time()-$s,1))")" | tee -a $ROOT/ab/out_e/runs.log
