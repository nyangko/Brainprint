#!/bin/bash
# Two rounds of four sessions; round 2 reverses round 1 (ABBA), so each (condition, order) runs twice.
P=$(dirname $0)/preflight.sh
R(){ $P > $(dirname $0)/out_f/preflight_$1.txt 2>&1 || echo "$1 PREFLIGHT_FAILED" >> $(dirname $0)/out_f/runs.log; $(dirname $0)/run_final.sh "$@"; }
mkdir -p $(dirname $0)/out_f
R A-O1-r1 A O1;  R B-O1-r1 B O1; R B-O2-r1 B O2; R A-O2-r1 A O2
R B-O2-r2 B O2; R A-O2-r2 A O2;  R A-O1-r2 A O1;  R B-O1-r2 B O1
echo ALL_DONE >> $(dirname $0)/out_f/runs.log
