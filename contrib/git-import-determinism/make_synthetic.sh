#!/bin/bash
# Deterministic synthetic Git repo exercising what `pijul git` handles
# specially: merges (clean, both sides editing, the same file added on both
# sides with equal and with different contents), renames (plain, with edit,
# whole directory), deletions, binary files (added, modified, renamed), an
# exec-bit change, a symlink, an empty commit, and a second root history merged
# with --allow-unrelated-histories.
set -euo pipefail
R=$1; rm -rf "$R"; mkdir -p "$R"; cd "$R"
export GIT_AUTHOR_NAME=Synth GIT_AUTHOR_EMAIL=synth@example.com
export GIT_COMMITTER_NAME=Synth GIT_COMMITTER_EMAIL=synth@example.com
t=1700000000
c() { t=$((t+60)); GIT_AUTHOR_DATE="@$t +0000" GIT_COMMITTER_DATE="@$t +0000" git commit -q "$@"; }
m() { t=$((t+60)); GIT_AUTHOR_DATE="@$t +0000" GIT_COMMITTER_DATE="@$t +0000" git merge -q --no-ff "$@"; }
bin() { python3 -c "import random,sys; random.seed($2); sys.stdout.buffer.write(bytes(random.getrandbits(8) for _ in range($3)))" > "$1"; }
lines() { for i in $(seq "$2" "$3"); do echo "$1 line $i"; done; }
git init -q -b main .
git config core.autocrlf false
mkdir -p src docs assets
lines a 1 40 > src/a.txt; lines b 1 30 > src/b.txt; lines readme 1 5 > README.md
lines doc 1 20 > docs/guide.md; bin assets/logo.bin 1 3000; printf '#!/bin/sh\necho hi\n' > run.sh
c --allow-empty-message -m "" 2>/dev/null || true
git add -A; c -m "initial"
sed -i 's/a line 10$/a line 10 edited/' src/a.txt; c -am "edit a"
lines c 1 10 > src/c.txt; git add src/c.txt; c -m "add c"
git mv src/b.txt src/b2.txt; c -m "rename b"
git mv src/c.txt src/c-renamed.txt; sed -i 's/c line 3$/c line 3 changed/' src/c-renamed.txt; git add -A; c -m "rename c with edit"
chmod +x run.sh; git add run.sh; c -m "exec bit"
ln -s src/a.txt link-to-a; git add link-to-a; c -m "symlink"
c --allow-empty -m "empty commit"
bin assets/logo.bin 2 3000; git add -A; c -m "binary modified"
# branch feature
git checkout -q -b feature
sed -i 's/a line 30$/a line 30 feature/' src/a.txt; c -am "feature: edit a end"
lines both 1 8 > shared-same.txt; git add shared-same.txt; c -m "feature: add shared-same"
lines feat 1 6 > shared-diff.txt; git add shared-diff.txt; c -m "feature: add shared-diff"
git mv docs guide-docs; c -m "feature: rename dir"
bin assets/photo.bin 3 5000; git add -A; c -m "feature: add binary"
sed -i 's/readme line 2$/readme line 2 same edit/' README.md; c -am "feature: same edit"
# main side
git checkout -q main
sed -i 's/a line 2$/a line 2 main/' src/a.txt; c -am "main: edit a start"
lines both 1 8 > shared-same.txt; git add shared-same.txt; c -m "main: add shared-same"
lines mainside 1 4 > shared-diff.txt; git add shared-diff.txt; c -m "main: add shared-diff"
sed -i 's/readme line 2$/readme line 2 same edit/' README.md; c -am "main: same edit"
git rm -q src/b2.txt; c -m "main: delete b2"
m feature -m "merge feature" -X ours 2>/dev/null || { lines resolved 1 3 > shared-diff.txt; git add -A; c -m "merge feature"; }
sed -i 's/a line 20$/a line 20 after merge/' src/a.txt; c -am "after merge edit"
git mv assets/photo.bin assets/picture.bin; c -m "rename binary"
# second root
git checkout -q --orphan other
git rm -rq --cached . ; git clean -fdxq
mkdir -p other; lines other 1 12 > other/o.txt; bin other/o.bin 4 2000; git add -A; c -m "other root"
sed -i 's/other line 5$/other line 5 x/' other/o.txt; c -am "other: edit"
git checkout -q main
m other --allow-unrelated-histories -m "merge other root"
sed -i 's/other line 7$/other line 7 y/' other/o.txt; c -am "edit other after merge"
# second merge round with a branch forked before the first merge
git checkout -q -b late HEAD~6
lines late 1 5 > late.txt; git add late.txt; c -m "late: add"
sed -i 's/a line 35$/a line 35 late/' src/a.txt; c -am "late: edit a"
git checkout -q main
m late -m "merge late"
lines z 1 3 > z.txt; git add z.txt; c -m "final"
git branch -D feature late other -q
git log --oneline --graph | head -60
