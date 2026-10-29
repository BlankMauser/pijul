#!/bin/bash
# Compare two convert.sh outputs. Exit 0 iff every record is identical.
# On a difference, names the first differing commit in processing order.
set -uo pipefail
A=$1; B=$2; rc=0
for f in commits.txt gitdb.txt log.txt changes.txt tree.txt; do
  if [ ! -f "$A/$f" ] || [ ! -f "$B/$f" ]; then
    [ "$f" = commits.txt ] && continue
    echo "MISSING $f"; rc=1; continue
  fi
  if cmp -s "$A/$f" "$B/$f"; then
    echo "same    $f ($(wc -l < "$A/$f") lines)"
  else
    echo "DIFFER  $f ($(diff "$A/$f" "$B/$f" | grep -c '^<') of $(wc -l < "$A/$f") lines)"; rc=1
  fi
done
if [ -f "$A/commits.txt" ] && [ -f "$B/commits.txt" ] && ! cmp -s "$A/commits.txt" "$B/commits.txt"; then
  first=$(paste -d'|' "$A/commits.txt" "$B/commits.txt" | awk -F'|' '$1!=$2{print NR; exit}')
  read -r n oid ha < <(sed -n "${first}p" "$A/commits.txt")
  read -r _ oidb hb < <(sed -n "${first}p" "$B/commits.txt")
  echo "first differing commit: #$n $oid \"$(git -C "$A/git" log -1 --format=%s "$oid" 2>/dev/null)\""
  echo "  parents: $(git -C "$A/git" log -1 --format=%P "$oid" 2>/dev/null)"
  echo "  A: $ha"; [ "$oidb" != "$oid" ] && echo "  B processed a different commit here: $oidb"; echo "  B: $hb"
fi
if [ -f "$A/gitdb.txt" ] && ! cmp -s "$A/gitdb.txt" "$B/gitdb.txt"; then
  echo "commits whose channel Merkle differs: $(join "$A/gitdb.txt" "$B/gitdb.txt" | awk '$2!=$3' | wc -l)"
fi
if [ $rc = 0 ]; then echo "IDENTICAL"; else echo "DIFFERENT"; fi
exit $rc
