#!/bin/bash
# usage: run_one.sh <name> <task> <arm A|B> <mode prefer|guard>
N=$1; T=$2; ARM=$3; MODE=$4; W=@T14_ROOT@/ab/wt/$N; O=@T14_ROOT@/ab/out/$N
PROMPT=$(python3 -c "import json;print(json.load(open('@T14_ROOT@/ab/prompts.json'))['$T'])")
BASEPATH=/Users/pixel/.cargo/bin:/usr/bin:/bin:/usr/sbin:/sbin
if [ "$ARM" = A ]; then P=$BASEPATH; S=@T14_ROOT@/ab/settings-A.json; M=@T14_ROOT@/ab/mcp-A.json; TEL=; else P=@T14_ROOT@/clean/repo/target/release:$BASEPATH; S=@T14_ROOT@/ab/settings-B-$MODE.json; M=@T14_ROOT@/ab/mcp-B.json; TEL=$O.telemetry.jsonl; fi
cd $W
s=$(python3 -c 'import time;print(time.time())')
env HOME=/Users/pixel PATH=$P XDG_RUNTIME_DIR=@T14_ROOT@/ab/run BRAINPRINT_ADOPTION_TELEMETRY_PATH=$TEL /Users/pixel/.local/bin/claude -p "$PROMPT" \
  --model claude-opus-5-5 --effort medium --setting-sources project --settings $S --strict-mcp-config --mcp-config $M \
  --permission-mode dontAsk --allowedTools "Read" "Grep" "Glob" "Bash" "Skill" "ToolSearch" "mcp__brainprint" \
  --disallowedTools "Edit" "Write" "NotebookEdit" "Agent" "Task" "WebFetch" "WebSearch" \
  --no-session-persistence --output-format stream-json --verbose < /dev/null > $O.stream.jsonl 2> $O.stderr
echo "$N exit=$? wall_s=$(python3 -c "import time;print(round(time.time()-$s,1))")" | tee -a @T14_ROOT@/ab/out/runs.log
git -C $W status --porcelain --untracked-files=all | grep -vE '^\?\? (\.brainprint/|\.claude/)' > $O.gitstatus
