#!/bin/bash
# Run the #32 analyzer (final_analyze.py, unchanged) once per arm X in A..E: arm N staged as its "A" condition,
# arm X as its "B" condition (same session ids otherwise). Output: <out>/N_vs_<X>.json + _calls.jsonl.
# usage: analyze_arms.sh <ab-root> <out-dir>
AB=$1; OUT=$2; mkdir -p $OUT
for X in A B C D E; do
  st=$(mktemp -d); mkdir -p $st/out_f $st/wtf
  for f in $AB/out_c/N-*.* $AB/out_c/$X-*.*; do
    b=$(basename $f); id=${b%%.*}; rest=${b#*.}; arm=${id%%-*}; tag=${id#*-}
    new=$([ $arm = N ] && echo A || echo B)-$tag
    ln -s $f $st/out_f/$new.$rest
    [ -d $AB/wtc/$id ] && [ ! -e $st/wtf/$new ] && ln -s $AB/wtc/$id $st/wtf/$new
  done
  (cd $AB && python3 final_analyze.py $st/out_f $OUT/N_vs_$X > $OUT/N_vs_$X.stdout 2>&1) || echo "analyze $X failed"
  rm -rf $st
done
