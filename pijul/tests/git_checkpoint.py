#!/usr/bin/env python3
"""Real-CLI checkpoint contract; no Cargo invocation and no implicit skips.

PIJUL_BIN=/private/build/pijul python3 pijul/tests/git_checkpoint.py
Each process is bounded; all scratch is private and below II_TEST_WORK.
The exported schema is a local-cache protocol, not a node/API format.
"""
import json
import os
import re
from pathlib import Path
import shutil
import subprocess
import tempfile
import tomllib
import unittest


class Checkpoints(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.pijul = str(Path(os.environ["PIJUL_BIN"]).resolve())
        work = Path(os.environ.get("II_TEST_WORK", Path.home() / ".cache/incremental-import/tests"))
        work.mkdir(parents=True, exist_ok=True, mode=0o700)
        cls.work = work

    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory(dir=self.work)
        self.addCleanup(self.tmp.cleanup)
        self.root = Path(self.tmp.name)
        self.git = self.root / "git"
        self.git.mkdir()
        self.env = dict(os.environ, HOME=str(self.root / "home"),
                        GIT_CONFIG_GLOBAL="/dev/null", GIT_CONFIG_NOSYSTEM="1",
                        GIT_AUTHOR_NAME="Ada", GIT_AUTHOR_EMAIL="ada@example.com",
                        GIT_COMMITTER_NAME="Ada", GIT_COMMITTER_EMAIL="ada@example.com",
                        GIT_AUTHOR_DATE="2000-01-01T00:00:00Z",
                        GIT_COMMITTER_DATE="2000-01-01T00:00:00Z")
        Path(self.env["HOME"]).mkdir()
        self.cmd("git", "init", "-q", "-b", "main", str(self.git))

    def cmd(self, *args, ok=True):
        result = subprocess.run(args, cwd=self.git, env=self.env, capture_output=True,
                                text=True, timeout=120)
        if ok:
            self.assertEqual(result.returncode, 0, f"{args}\n{result.stdout}\n{result.stderr}")
        else:
            self.assertNotEqual(result.returncode, 0, f"unexpected success: {args}")
        return result

    def commit(self, name, content):
        (self.git / name).write_text(content)
        self.cmd("git", "add", "-A")
        self.cmd("git", "commit", "-qm", f"write {name}: {content}")
        return self.cmd("git", "rev-parse", "HEAD").stdout.strip()

    def convert(self, destination, seed=None):
        exported = destination.with_suffix(".json")
        args = [self.pijul, "git", str(self.git), str(destination),
                "--import-state-out", str(exported)]
        if seed:
            args += ["--import-state-in", str(seed)]
        self.cmd(*args)
        result = json.loads(exported.read_text())
        self.assertEqual(result["schema"], 1)
        return exported, result

    def assert_parity(self, resumed):
        full_root = Path(tempfile.mkdtemp(prefix="full-", dir=self.root))
        _, full = self.convert(full_root)
        # A mismatch is a test FAILURE, never a performance diagnostic.
        self.assertEqual(resumed["commits"], full["commits"])
        self.assertEqual(resumed["head"], full["head"])

    def test_late_pr_restores_a_deleted_old_fork_point(self):
        fork = self.commit("root.txt", "root\n")
        self.commit("main.txt", "main\n")
        raw = self.root / "raw"
        seed, first = self.convert(raw)
        self.cmd("git", "checkout", "-qb", "pr", fork)
        side = self.commit("pr.txt", "side\n")
        self.cmd("git", "checkout", "-q", "main")
        self.cmd("git", "merge", "--no-ff", "-qm", "merge old PR", "pr")
        merge = self.cmd("git", "rev-parse", "HEAD").stdout.strip()
        _, resumed = self.convert(raw, seed)
        self.assertEqual(set(resumed["commits"]) - set(first["commits"]), {side, merge})
        self.assertEqual(resumed["converted"], 2)
        self.assert_parity(resumed)

    def test_linear_and_same_head_do_not_record_old_commits(self):
        self.commit("a", "a\n")
        raw = self.root / "raw"
        seed, _ = self.convert(raw)
        self.commit("a", "changed\n")
        seed, second = self.convert(raw, seed)
        self.assertEqual(second["converted"], 1)
        self.assert_parity(second)
        _, third = self.convert(raw, seed)
        self.assertEqual(third["converted"], 0)
        self.assertEqual(second["commits"], third["commits"])
        self.assertEqual((raw / "a").read_text(), "changed\n")

    def test_empty_root_and_noop_commit_have_checkpoints(self):
        self.cmd("git", "commit", "--allow-empty", "-qm", "empty root")
        raw = self.root / "raw"
        seed, initial = self.convert(raw)
        self.assertEqual(len(initial["commits"]), 1)
        self.cmd("git", "commit", "--allow-empty", "-qm", "still empty")
        _, resumed = self.convert(raw, seed)
        self.assertEqual(resumed["converted"], 1)
        self.assert_parity(resumed)

    def test_corrupt_checkpoint_is_refused_without_completed_export(self):
        self.commit("a", "a\n")
        raw = self.root / "raw"
        seed, state = self.convert(raw)
        state["commits"][state["head"]]["state"] = "invalid-state"
        seed.write_text(json.dumps(state))
        self.commit("b", "b\n")
        output = self.root / "bad-export.json"
        self.cmd(self.pijul, "git", str(self.git), str(raw),
                 "--import-state-in", str(seed), "--import-state-out", str(output), ok=False)
        self.assertFalse(output.exists())

    def test_repeated_imports_of_prs_from_the_original_root(self):
        fork = self.commit("root", "root\n")
        self.commit("main", "main\n")
        raw = self.root / "raw"
        seed, _ = self.convert(raw)
        for n in range(5):
            branch = f"pr-{n}"
            self.cmd("git", "checkout", "-qb", branch, fork)
            self.commit(f"side-{n}", f"side {n}\n")
            self.cmd("git", "checkout", "-q", "main")
            self.cmd("git", "merge", "--no-ff", "-qm", f"merge {n}", branch)
            seed, resumed = self.convert(raw, seed)
            self.assertEqual(resumed["converted"], 2)
            self.assert_parity(resumed)

    def test_export_does_not_change_plain_conversion(self):
        self.commit("a", "one\n")
        self.commit("a", "two\n")
        exported = self.root / "exported"
        self.convert(exported)
        plain = self.root / "plain"
        self.cmd(self.pijul, "git", str(self.git), str(plain))
        def changes(path):
            root = path / ".pijul/changes"
            return {str(p.relative_to(root)): p.read_bytes()
                    for p in root.rglob("*") if p.is_file()}
        self.assertEqual(changes(exported), changes(plain))

    def test_conflicting_merge_and_rename_have_full_parity(self):
        fork = self.commit("a", "base\n")
        raw = self.root / "raw"
        seed, _ = self.convert(raw)
        self.cmd("git", "checkout", "-qb", "side", fork)
        self.commit("a", "side\n")
        self.cmd("git", "checkout", "-q", "main")
        self.commit("a", "main\n")
        self.cmd("git", "merge", "--no-ff", "-qm", "conflict", "side", ok=False)
        self.commit("a", "resolved\n")
        self.cmd("git", "mv", "a", "renamed")
        self.cmd("git", "commit", "-qm", "rename")
        _, resumed = self.convert(raw, seed)
        self.assertEqual(resumed["converted"], 4)
        self.assert_parity(resumed)

    def test_unrelated_root_merge_has_full_parity(self):
        self.commit("a", "main\n")
        raw = self.root / "raw"
        seed, _ = self.convert(raw)
        self.cmd("git", "checkout", "-q", "--orphan", "vendor")
        self.cmd("git", "rm", "-rfq", ".")
        self.commit("vendor", "unrelated\n")
        self.cmd("git", "checkout", "-q", "main")
        self.cmd("git", "merge", "--allow-unrelated-histories", "--no-ff", "-qm", "vendor", "vendor")
        self.commit("later", "after merge\n")
        _, resumed = self.convert(raw, seed)
        self.assertEqual(resumed["converted"], 3)
        self.assert_parity(resumed)

    def test_copied_raw_cache_has_identical_output(self):
        self.commit("a", "a\n")
        one = self.root / "one"
        seed, _ = self.convert(one)
        two = self.root / "two"
        shutil.copytree(one, two)
        original_seed = self.root / "original-seed.json"
        shutil.copyfile(seed, original_seed)
        self.commit("a", "b\n")
        _, first = self.convert(one, seed)
        self.assertEqual(first["converted"], 1)
        _, second = self.convert(two, original_seed)
        self.assertEqual(first["commits"], second["commits"])
        self.assert_parity(first)


    def log_hashes(self, repository):
        result = subprocess.run([self.pijul, "log", "--hash-only"], cwd=repository, env=self.env,
                                capture_output=True, text=True, timeout=120)
        self.assertEqual(result.returncode, 0, result.stderr)
        return result.stdout.split()

    def test_checkpoint_over_the_limit_does_not_fail_the_import(self):
        """A history whose recipes exceed the size limit stays importable: the
        conversion is the normal one and no checkpoint is exported (or left
        stale). The limit is lowered through a test-only environment hook."""
        for n in range(3):
            self.commit(f"f{n}", f"{n}\n")
        plain = self.root / "plain"
        self.cmd(self.pijul, "git", str(self.git), str(plain))
        raw = self.root / "raw"
        exported = raw.with_suffix(".json")
        exported.write_text("stale checkpoint from an older run")
        self.env["PIJUL_GIT_CHECKPOINT_MAX_BYTES"] = "64"
        result = self.cmd(self.pijul, "git", str(self.git), str(raw),
                          "--import-state-out", str(exported))
        del self.env["PIJUL_GIT_CHECKPOINT_MAX_BYTES"]
        self.assertFalse(exported.exists(), "an oversized or stale checkpoint was left")
        self.assertIn("without a checkpoint", result.stderr)
        self.assertIn("size limit", result.stderr)
        self.assertEqual(self.log_hashes(raw), self.log_hashes(plain))
        self.assertEqual(len(self.log_hashes(raw)), 3)
        # With the real limit the same history exports as before.
        self.convert(self.root / "normal")

    def test_packaged_cli_includes_the_checkpoint_module(self):
        manifest = tomllib.loads((Path(__file__).resolve().parents[1] / "Cargo.toml").read_text())
        included = set(manifest["package"]["include"])
        self.assertIn("src/commands/git_checkpoint.rs", included)
        # Every module reached through #[path] must be packaged too.
        source = Path(__file__).resolve().parents[1] / "src/commands"
        for file in source.glob("*.rs"):
            for target in re.findall(r'#\[path = "([^"]+)"\]', file.read_text()):
                self.assertIn(f"src/commands/{target}", included, f"{file.name} -> {target}")


if __name__ == "__main__":
    unittest.main()
