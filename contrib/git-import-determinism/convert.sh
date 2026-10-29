#!/bin/bash
# Convert one Git repository with one `pijul` binary and record everything a
# re-conversion must reproduce byte for byte.
#
#   convert.sh PIJUL_BIN GIT_REPO OUT_DIR
#
# GIT_REPO is cloned (like `ruttr import git`: --single-branch --no-tags) to
# OUT_DIR/git, converted with `PIJUL_BIN git OUT_DIR/git OUT_DIR/pijul`, then:
#   OUT_DIR/commits.txt  per processed commit, in processing order:
#                        "<n> <git oid> <change hash or ->" (from --stats)
#   OUT_DIR/gitdb.txt    per commit "<git oid> <channel Merkle>" (.pijul/git/db)
#   OUT_DIR/log.txt      final channel log with states (pijul log --state)
#   OUT_DIR/changes.txt  sha256 of every change file
#   OUT_DIR/tree.txt     sha256 + mode of every output file
#   OUT_DIR/run.txt      wall seconds, rchar, read_bytes (/proc/<pid>/io)
# With STATS=0 the conversion runs without --stats (timing runs); commits.txt
# is then not written.
set -euo pipefail
PIJUL=$(realpath "$1"); SRC=$2; OUT=$3
HERE=$(cd "$(dirname "$0")" && pwd)
DUMP=${GITDB_DUMP:-${CARGO_TARGET_DIR:-$HERE/gitdb-dump/target}/release/gitdb-dump}
rm -rf "$OUT"; mkdir -p "$OUT"
if [ -d "$SRC" ]; then local_flag=--no-local; else local_flag=; fi
GIT_LFS_SKIP_SMUDGE=1 git clone --quiet --single-branch --no-tags $local_flag -- "$SRC" "$OUT/git"
mkdir -p "$OUT/pijul"
args=(git "$OUT/git" "$OUT/pijul")
if [ "${STATS:-1}" = 1 ]; then args+=(--stats "$OUT/stats.csv"); fi
start=$(date +%s%N)
RUST_LOG=${RUST_LOG:-pijul::commands::git=info} "$PIJUL" "${args[@]}" > "$OUT/stdout.txt" 2> "$OUT/stderr.txt" &
pid=$!
io=""
while kill -0 $pid 2>/dev/null; do
  cur=$(cat /proc/$pid/io 2>/dev/null || true)
  [ -n "$cur" ] && io=$cur
  sleep 0.2
done
wait $pid || { echo "pijul git failed, see $OUT/stderr.txt" >&2; tail -5 "$OUT/stderr.txt" >&2; exit 1; }
end=$(date +%s%N)
{
  echo "wall_s $(awk -v a=$start -v b=$end 'BEGIN{printf "%.2f", (b-a)/1e9}')"
  echo "$io" | grep -E '^(rchar|read_bytes|wchar|write_bytes):' | tr -d ':'
  echo "ulimit_n_soft $(ulimit -Sn) hard $(ulimit -Hn)"
} > "$OUT/run.txt"
if [ -f "$OUT/stats.csv" ]; then
  # stats columns: child oid, n, ..., hash (last; empty when nothing recorded)
  awk -F', ' '{h=$NF; if (h=="") h="-"; print $2, $1, h}' "$OUT/stats.csv" > "$OUT/commits.txt"
fi
"$DUMP" "$OUT/pijul/.pijul/git/db" > "$OUT/gitdb.txt"
(cd "$OUT/pijul" && "$PIJUL" log --state > "$OUT/log.txt")
(cd "$OUT/pijul/.pijul/changes" && find . -type f | sort | xargs -r sha256sum) > "$OUT/changes.txt"
(cd "$OUT/pijul" && find . -path ./.pijul -prune -o -type f -printf '%m %p\n' | sort -k2 | while read -r m p; do echo "$m $(sha256sum < "$p" | cut -c1-64) $p"; done) > "$OUT/tree.txt"
echo "$OUT: $(wc -l < "$OUT/gitdb.txt") commits, $(wc -l < "$OUT/changes.txt") changes, $(head -1 "$OUT/run.txt")"
