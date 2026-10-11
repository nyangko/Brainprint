#!/bin/bash
# #96 round r1, pre-registered before any measured session: O1 N H M, then O2 M H N.
cd "$(dirname "$0")"
for s in "N-O1-r1 N O1" "H-O1-r1 H O1" "M-O1-r1 M O1" "M-O2-r1 M O2" "H-O2-r1 H O2" "N-O2-r1 N O2"; do
  ./run.sh $s
done
