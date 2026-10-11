#!/bin/bash
# #96 round r2 after the r1 review (callee on further hops, `knowledge rules` in CLI.md): H2 = H on that
# product. Arm order reversed against r1: O1 H2 N, then O2 N H2. M is not rerun (unchanged product path).
cd "$(dirname "$0")"
for s in "H2-O1-r2 H2 O1" "N-O1-r2 N O1" "N-O2-r2 N O2" "H2-O2-r2 H2 O2"; do ./run.sh $s; done
true
