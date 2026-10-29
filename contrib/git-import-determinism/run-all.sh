#!/bin/bash
# Convert every corpus repo with OLD and NEW pijul and compare.
#   run-all.sh OLD_BIN NEW_BIN [WORK] [repos...]
set -uo pipefail
HERE=$(cd "$(dirname "$0")" && pwd)
OLD=$1; NEW=$2; W=${3:-$HOME/.cache/pijul-git-speed/runs}; shift 3 || shift $#
C=${CORPUS:-$HOME/.cache/pijul-git-speed/corpus}
repos=("$@"); [ ${#repos[@]} = 0 ] && repos=(synthetic ryvfs usagui)
rc=0
for r in "${repos[@]}"; do
  "$HERE/convert.sh" "$OLD" "$C/$r" "$W/$r.old" || { rc=1; continue; }
  "$HERE/convert.sh" "$NEW" "$C/$r" "$W/$r.new" || { rc=1; continue; }
  echo "== $r"; "$HERE/compare.sh" "$W/$r.old" "$W/$r.new" || rc=1
done
exit $rc
