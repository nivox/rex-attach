"""Exercise release commits and pushes using disposable local repositories."""

from pathlib import Path
import subprocess
import sys
import tempfile
import tomllib
import unittest

import release


SCRIPT = Path(release.__file__).resolve()


class ReleaseTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory(prefix="rex-release-test-")
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name)
        self.repo = self.root / "repo"
        self.repo.mkdir()
        self.git("init", "-b", "main")
        for key, value in (
            ("user.name", "Release Test"),
            ("user.email", "release@example.invalid"),
            ("commit.gpgsign", "false"),
            ("tag.gpgsign", "false"),
            ("core.hooksPath", str(self.root / "no-hooks")),
        ):
            self.git("config", key, value)
        (self.repo / "Cargo.toml").write_text(
            '[package]\nname = "rex-attach"\nversion = "0.1.0" # keep comment\n'
        )
        (self.repo / "Cargo.lock").write_text(
            '# keep lock header\nversion = 4\n\n'
            '[[package]]\nname = "dependency"\nversion = "9.8.7"\n\n'
            '[[package]]\nname = "rex-attach"\nversion = "0.1.0"\n'
        )
        self.commit()

    def git(self, *args):
        return release.git(self.repo, *args)

    def commit(self):
        self.git("add", ".")
        self.git("commit", "-m", "fixture")

    def run_release(self, *args, success=True):
        result = subprocess.run(
            [sys.executable, str(SCRIPT), "--yes", *args],
            cwd=self.repo, text=True, capture_output=True,
        )
        self.assertEqual(result.returncode == 0, success, result.stdout + result.stderr)
        return result

    def assert_versions(self, revision, expected):
        manifest = tomllib.loads(self.git("show", f"{revision}:Cargo.toml"))
        lock = tomllib.loads(self.git("show", f"{revision}:Cargo.lock"))
        self.assertEqual(manifest["package"]["version"], expected)
        self.assertEqual(lock["package"][1]["version"], expected)
        self.assertEqual(lock["package"][0]["version"], "9.8.7")
        self.assertIn("# keep comment", self.git("show", f"{revision}:Cargo.toml"))
        self.assertIn("# keep lock header", self.git("show", f"{revision}:Cargo.lock"))

    def test_first_release_and_development_resume(self):
        self.run_release()
        self.assert_versions("v0.1.0", "0.1.0")
        self.assert_versions("HEAD", "0.2.0-dev")
        self.assertEqual(self.git("cat-file", "-t", "refs/tags/v0.1.0"), "tag")
        self.assertEqual(self.git("rev-parse", "HEAD^"), self.git("rev-parse", "v0.1.0^{}"))
        self.assertEqual(self.git("status", "--porcelain"), "")

    def test_dev_release_uses_current_version_without_suffix(self):
        release.set_version(self.repo, "rex-attach", "0.2.0-dev")
        self.commit()
        self.run_release()
        self.assert_versions("v0.2.0", "0.2.0")
        self.assert_versions("HEAD", "0.3.0-dev")

    def test_explicit_versions(self):
        self.run_release("--version", "1.2.3", "--next", "1.3.1")
        self.assert_versions("v1.2.3", "1.2.3")
        self.assert_versions("HEAD", "1.3.1-dev")

    def test_dry_run_makes_no_changes(self):
        original = self.git("rev-parse", "HEAD")
        self.run_release("--dry-run")
        self.assertEqual(self.git("rev-parse", "HEAD"), original)
        self.assertEqual(self.git("tag", "--list"), "")
        self.assertEqual(self.git("status", "--porcelain"), "")

    def test_declined_confirmation_makes_no_changes(self):
        original = self.git("rev-parse", "HEAD")
        result = subprocess.run(
            [sys.executable, str(SCRIPT)], cwd=self.repo,
            input="n\n", text=True, capture_output=True,
        )
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("aborted", result.stderr)
        self.assertEqual(self.git("rev-parse", "HEAD"), original)
        self.assertEqual(self.git("tag", "--list"), "")
        self.assertEqual(self.git("status", "--porcelain"), "")

    def test_invalid_versions_and_existing_tags_fail_without_edits(self):
        original = self.git("rev-parse", "HEAD")
        for args in (
            ("--version", "01.2.3"), ("--version", "0.0.9"),
            ("--next", "0.1.0"), ("--next", "0.0.9"),
        ):
            with self.subTest(args=args):
                self.run_release(*args, success=False)
                self.assertEqual(self.git("rev-parse", "HEAD"), original)
                self.assertEqual(self.git("status", "--porcelain"), "")
        self.git("tag", "v0.1.0")
        self.assertIn("already exists", self.run_release(success=False).stderr)
        self.assertEqual(self.git("rev-parse", "HEAD"), original)

    def test_dirty_tree_detached_head_and_lock_mismatch_are_rejected(self):
        (self.repo / "untracked").touch()
        self.assertIn("not clean", self.run_release(success=False).stderr)
        (self.repo / "untracked").unlink()
        self.git("checkout", "--detach")
        self.assertIn("HEAD is detached", self.run_release(success=False).stderr)
        self.git("checkout", "main")
        path = self.repo / "Cargo.lock"
        path.write_text(path.read_text().replace('version = "0.1.0"', 'version = "0.0.9"'))
        self.commit()
        self.assertIn("do not match", self.run_release(success=False).stderr)

    def test_push_updates_branch_and_tag_on_local_remote(self):
        remote = self.root / "origin.git"
        subprocess.run(["git", "init", "--bare", str(remote)], check=True, capture_output=True)
        self.git("remote", "add", "origin", str(remote))
        self.run_release("--push")
        self.assertEqual(
            release.git(remote, "rev-parse", "refs/heads/main"), self.git("rev-parse", "HEAD")
        )
        self.assertEqual(
            release.git(remote, "rev-parse", "v0.1.0^{}"), self.git("rev-parse", "HEAD^")
        )

    def test_existing_remote_tag_is_rejected_before_local_edits(self):
        remote = self.root / "origin.git"
        subprocess.run(["git", "init", "--bare", str(remote)], check=True, capture_output=True)
        self.git("remote", "add", "origin", str(remote))
        self.git("tag", "v0.1.0")
        self.git("push", "origin", "refs/tags/v0.1.0")
        self.git("tag", "-d", "v0.1.0")
        original = self.git("rev-parse", "HEAD")
        self.assertIn("already exists on origin", self.run_release("--push", success=False).stderr)
        self.assertEqual(self.git("rev-parse", "HEAD"), original)
        self.assertEqual(self.git("tag", "--list"), "")

    def test_atomic_push_rejection_does_not_publish_tag(self):
        remote = self.root / "origin.git"
        subprocess.run(["git", "init", "--bare", str(remote)], check=True, capture_output=True)
        self.git("remote", "add", "origin", str(remote))
        original = self.git("rev-parse", "HEAD")
        # Simulate origin moving ahead while this checkout remains behind.
        self.git("commit", "--allow-empty", "-m", "remote change")
        remote_head = self.git("rev-parse", "HEAD")
        self.git("push", "origin", "HEAD:refs/heads/main")
        self.git("reset", "--hard", original)
        self.run_release("--push", success=False)
        self.assertEqual(release.git(remote, "rev-parse", "refs/heads/main"), remote_head)
        self.assertEqual(release.git(remote, "tag", "--list"), "")


if __name__ == "__main__":
    unittest.main()
