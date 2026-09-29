#!/bin/bash
# ABBA-style rounds per task so each condition sees the same drift: 4 runs per condition per task.
R=@T14_ROOT@/ab/run_instr.sh
for T in T1; do
  n=0
  for round in 1 2 3 4; do
    if [ $((round % 2)) = 1 ]; then ORDER="A B0 B1"; else ORDER="B1 B0 A"; fi
    for C in $ORDER; do n=$((n+1)); $R $T-$C-r$round $T $C; done
  done
done
echo ALL_DONE >> @T14_ROOT@/ab/out_i/runs.log
