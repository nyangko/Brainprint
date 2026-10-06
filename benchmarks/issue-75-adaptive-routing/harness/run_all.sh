#!/bin/bash
# Two rounds; arm order N B R, then R B N (time drift spread over the arms).
D=$(dirname $0)
for a in N B R; do $D/run_arm.sh $a r1; done
for a in R B N; do $D/run_arm.sh $a r2; done
echo ALL_DONE >> @T75_ROOT@/ab/out/runs.log
