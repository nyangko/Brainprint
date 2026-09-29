#!/bin/bash
R=@T14_ROOT@/ab/run_e.sh
# same worktree, back to back: does the 2nd run read the 1st run's prompt cache?
$R T1-B-seq1 T1-B-r2 T1 B prefer 0; $R T1-B-seq2 T1-B-r2 T1 B prefer 0
$R T1-A-seq1 T1-A-r1 T1 A prefer 0; $R T1-A-seq2 T1-A-r1 T1 A prefer 0
$R T2-B-seq1 T2-B-r2 T2 B prefer 0; $R T2-B-seq2 T2-B-r2 T2 B prefer 0
$R T2-A-seq1 T2-A-r1 T2 A prefer 0; $R T2-A-seq2 T2-A-r1 T2 A prefer 0
echo PAIRS_DONE >> @T14_ROOT@/ab/out_e/runs.log
