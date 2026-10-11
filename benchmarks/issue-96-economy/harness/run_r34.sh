#!/bin/bash
# #96 rounds r3/r4, pre-registered before running: H2 vs N only, product b0d2222 unchanged.
# r3: O1 N H2, O2 H2 N; r4: O1 H2 N, O2 N H2 (arm order alternated against r2).
cd "$(dirname "$0")"
for s in "N-O1-r3 N O1" "H2-O1-r3 H2 O1" "H2-O2-r3 H2 O2" "N-O2-r3 N O2" \
         "H2-O1-r4 H2 O1" "N-O1-r4 N O1" "N-O2-r4 N O2" "H2-O2-r4 H2 O2"; do ./run.sh $s; done
true
