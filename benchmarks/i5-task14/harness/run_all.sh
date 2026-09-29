#!/bin/bash
R1=@T14_ROOT@/ab/run_one.sh
$R1 T1-A-r1 T1 A prefer; $R1 T1-B-r1 T1 B prefer; $R1 T1-B-r2 T1 B prefer; $R1 T1-A-r2 T1 A prefer
$R1 T2-A-r1 T2 A prefer; $R1 T2-B-r1 T2 B prefer; $R1 T2-B-r2 T2 B prefer; $R1 T2-A-r2 T2 A prefer
$R1 T1-Bg T1 B guard; $R1 T2-Bg T2 B guard
echo ALL_DONE >> @T14_ROOT@/ab/out/runs.log
