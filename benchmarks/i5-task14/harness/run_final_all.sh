#!/bin/bash
# Two rounds of four sessions; round 2 reverses round 1 (ABBA), so each (condition, order) runs twice.
R=$(dirname $0)/run_final.sh
$R A-O1-r1 A O1;  $R B-O1-r1 B O1; $R B-O2-r1 B O2; $R A-O2-r1 A O2
$R B-O2-r2 B O2; $R A-O2-r2 A O2;  $R A-O1-r2 A O1;  $R B-O1-r2 B O1
echo ALL_DONE >> $(dirname $0)/out_f/runs.log
