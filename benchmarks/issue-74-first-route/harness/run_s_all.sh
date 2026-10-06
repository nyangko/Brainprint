#!/bin/bash
# Two rounds; arm order alternates so time drift spreads over both arms.
D=$(dirname $0)
for x in "N-O1-r1 N O1" "S-O1-r1 S O1" "S-O2-r1 S O2" "N-O2-r1 N O2" "S-O1-r2 S O1" "N-O1-r2 N O1" "N-O2-r2 N O2" "S-O2-r2 S O2"; do $D/run_s.sh $x; done
echo ALL_DONE >> @T74_ROOT@/ab/out_s/runs.log
