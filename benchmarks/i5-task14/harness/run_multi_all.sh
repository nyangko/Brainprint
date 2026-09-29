#!/bin/bash
# Two rounds of four sessions. Each round holds A and B1 once in each task order (O1, O2), and the second
# round reverses the first, so drift in machine load / prompt-cache state is spread over both conditions and
# both orders: 2 sessions per (condition, order), 4 per condition, 8 in total.
R=@T14_ROOT@/ab/run_multi.sh
$R A-O1-r1 A O1;  $R B1-O1-r1 B1 O1; $R B1-O2-r1 B1 O2; $R A-O2-r1 A O2
$R B1-O2-r2 B1 O2; $R A-O2-r2 A O2;  $R A-O1-r2 A O1;  $R B1-O1-r2 B1 O1
echo ALL_DONE >> @T14_ROOT@/ab/out_m/runs.log
