#!/bin/bash
# Round 1: O1 in arm order N A B C D E, then O2 in reverse arm order (time drift spread over the arms).
D=$(dirname $0); mkdir -p @T14_ROOT@/ab/out_c
P(){ @T14_ROOT@/ab/preflight.sh > @T14_ROOT@/ab/out_c/preflight_$1.txt 2>&1 || echo "$1 PREFLIGHT_FAILED" >> @T14_ROOT@/ab/out_c/runs.log; }
for a in N A B C D E; do P $a-O1-r1; $D/run_contract.sh $a-O1-r1 $a O1; done
for a in E D C B A N; do P $a-O2-r1; $D/run_contract.sh $a-O2-r1 $a O2; done
echo ALL_DONE >> @T14_ROOT@/ab/out_c/runs.log
