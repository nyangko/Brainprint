#!/bin/bash
# Round 2 for the leading candidate only (B) against the shipped A and native N: O1 then O2, arm order reversed.
D=$(dirname $0); OUT=@T14_ROOT@/ab/out_c
P(){ @T14_ROOT@/ab/preflight.sh > $OUT/preflight_$1.txt 2>&1 || echo "$1 PREFLIGHT_FAILED" >> $OUT/runs.log; }
for a in A B N; do P $a-O1-r2; $D/run_contract.sh $a-O1-r2 $a O1; done
for a in N B A; do P $a-O2-r2; $D/run_contract.sh $a-O2-r2 $a O2; done
echo ALL_DONE_R2 >> $OUT/runs.log
