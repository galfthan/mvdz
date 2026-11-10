#!/bin/bash
# CLI-path benchmark: every codec is timed the same way, one process per
# file, reading the compressed file and writing the decoded demo to a file
# (tmpfs by default), as a consumer would. Baselines: the archive's own
# gzip files (when ARCHIVE is set), xz -9 and zstd -19 made here.
#
#   scripts/bench_cli.sh WORKDIR FILELIST     (FILELIST: one .mvd path per line)
#
# Environment: MVDZ (binary, default target/release/mvdz), ARCHIVE (dir of
# <name>.gz), OUT (decoded output, default /dev/shm/mvdz-bench.out), REPS
# (default 3). Prints, per codec: bytes, ratio to raw, ratio to gz, and
# decode MB/s (MB = 10^6 bytes of raw demo) for 1 thread and for a batch of
# 4 processes in parallel: median [min-max] over REPS runs.
set -eu
W=$1
LIST=$2
MVDZ=${MVDZ:-target/release/mvdz}
OUT=${OUT:-/dev/shm/mvdz-bench.out}
REPS=${REPS:-3}
mkdir -p "$W"/normal "$W"/fast "$W"/xz "$W"/zstd "$W"/gz
mapfile -t FILES < "$LIST"
raw=0
for f in "${FILES[@]}"; do raw=$((raw + $(stat -c %s "$f"))); done
# compress once (sizes; 4 files at a time); compression timing is the
# library bench's job
prep() {
  n=$(basename "$1")
  [ -s "$W/normal/$n.mvdz" ] || "$MVDZ" compress -s 0 -j 1 "$1" "$W/normal/$n.mvdz"
  [ -s "$W/fast/$n.mvdz" ] || "$MVDZ" compress --fast -s 0 -j 1 "$1" "$W/fast/$n.mvdz"
  [ -s "$W/xz/$n.xz" ] || xz -9 -T1 -c "$1" > "$W/xz/$n.xz"
  [ -s "$W/zstd/$n.zst" ] || zstd -q -19 -c "$1" > "$W/zstd/$n.zst"
  if [ -n "${ARCHIVE:-}" ] && [ ! -s "$W/gz/$n.gz" ]; then cp "$ARCHIVE/$n.gz" "$W/gz/$n.gz"; fi
}
export -f prep
export W MVDZ ARCHIVE="${ARCHIVE:-}"
printf '%s\n' "${FILES[@]}" | xargs -P 4 -I{} bash -c 'prep "$1"' _ {}
dec() { # codec file
  case $1 in
    normal|fast) "$MVDZ" decompress -j 1 "$2" "$OUT.$$" ;;
    xz) xz -dc "$2" > "$OUT.$$" ;;
    zstd) zstd -qdc "$2" > "$OUT.$$" ;;
    gz) gzip -dc "$2" > "$OUT.$$" ;;
  esac
  rm -f "$OUT.$$"
}
export -f dec
export MVDZ OUT
stats() { sort -g | awk '{a[NR]=$1} END {m=(NR%2)?a[(NR+1)/2]:(a[NR/2]+a[NR/2+1])/2; printf "%.1f [%.1f-%.1f]", m, a[1], a[NR]}'; }
gzbytes=$(cat "$W"/gz/* 2>/dev/null | wc -c)
printf 'files %d  raw %.2f MB (10^6 bytes)  reps %d  load %s\n' ${#FILES[@]} "$(echo "$raw/1000000" | bc -l)" $REPS "$(cut -d' ' -f1-3 /proc/loadavg)"
printf '%-7s %12s %8s %8s  %-22s %-22s\n' codec bytes ratio "vs gz" "decode 1t MB/s" "decode batch(4) MB/s"
for c in normal fast zstd xz gz; do
  ls "$W/$c" | grep -q . || continue
  bytes=$(cat "$W/$c"/* | wc -c)
  one=(); bat=()
  for r in $(seq $REPS); do
    t0=$(date +%s.%N)
    for f in "$W/$c"/*; do dec $c "$f"; done
    t1=$(date +%s.%N)
    one+=("$(echo "$raw / 1000000 / ($t1 - $t0)" | bc -l)")
    t0=$(date +%s.%N)
    ls "$W/$c"/* | xargs -P 4 -I{} bash -c 'dec '$c' "$1"' _ {}
    t1=$(date +%s.%N)
    bat+=("$(echo "$raw / 1000000 / ($t1 - $t0)" | bc -l)")
  done
  vsgz=$( [ "$gzbytes" -gt 0 ] && echo "$bytes / $gzbytes" | bc -l || echo 0)
  printf '%-7s %12d %8.4f %8.4f  %-22s %-22s\n' $c $bytes "$(echo "$bytes / $raw" | bc -l)" "$vsgz" \
    "$(printf '%s\n' "${one[@]}" | stats)" "$(printf '%s\n' "${bat[@]}" | stats)"
done
rm -f "$OUT".*
echo "load after: $(cut -d' ' -f1-3 /proc/loadavg)"
