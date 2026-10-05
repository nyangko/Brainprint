#!/bin/bash
# Resume of run_final_all.sh after A-O1-r1 (same order).
R=$(dirname $0)/run_final.sh
$R B-O1-r1 B O1; $R B-O2-r1 B O2; $R A-O2-r1 A O2
$R B-O2-r2 B O2; $R A-O2-r2 A O2;  $R A-O1-r2 A O1;  $R B-O1-r2 B O1
echo ALL_DONE >> $(dirname $0)/out_f/runs.log
