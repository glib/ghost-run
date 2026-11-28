"""scripts/release.py: versions from Conventional Commits, on throwaway git repos."""

import importlib.util
import subprocess
import tempfile
import unittest
from pathlib import Path

SCRIPT = Path(__file__).resolve().parents[1] / "scripts/release.py"
spec = importlib.util.spec_from_file_location("release", SCRIPT)
release = importlib.util.module_from_spec(spec)
spec.loader.exec_module(release)


class Repo:
    def __init__(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.path = self.tmp.name
        self.git("init", "-q", "-b", "main")

    def git(self, *args):
        subprocess.run(
            ["git", "-c", "user.name=t", "-c", "user.email=t@example.com", *args],
            cwd=self.path,
            check=True,
            capture_output=True,
        )

    def commit(self, message):
        self.git("commit", "-q", "--allow-empty", "-m", message)

    def tag(self, name):
        self.git("tag", name)

    def next(self):
        return release.next_release(self.path)


class TitleTest(unittest.TestCase):
    def test_accepts_conventional_titles(self):
        for title in [
            "feat: add the updater",
            "fix(import): a card with no DCIM",
            "feat(library, videos)!: drop schema 5",
            "docs: releasing",
        ]:
            self.assertIsNone(release.check_title(title), title)

    def test_rejects_others(self):
        for title in ["Add the updater", "feature: x", "feat:x", "feat: ", "Feat: x"]:
            self.assertIsNotNone(release.check_title(title), title)
        self.assertIn("isn't a kind", release.check_title("feature: x"))

    def test_bumps(self):
        self.assertEqual(release.bump_of("feat: x"), "minor")
        self.assertEqual(release.bump_of("fix: x"), "patch")
        self.assertEqual(release.bump_of("perf(render): x"), "patch")
        self.assertEqual(release.bump_of("feat!: x"), "major")
        self.assertEqual(release.bump_of("fix: x", "Why.\n\nBREAKING CHANGE: y"), "major")
        self.assertIsNone(release.bump_of("docs: x"))
        self.assertIsNone(release.bump_of("ci: x"))
        # Not a Conventional Commit: it changed something, so a patch.
        self.assertEqual(release.bump_of("Previews: kept on the NAS (#68)"), "patch")
        self.assertEqual(release.bump((1, 2, 3), "major"), (2, 0, 0))
        self.assertEqual(release.bump((1, 2, 3), "minor"), (1, 3, 0))
        self.assertEqual(release.bump((1, 2, 3), "patch"), (1, 2, 4))


class NextTest(unittest.TestCase):
    def repo(self):
        repo = Repo()
        self.addCleanup(repo.tmp.cleanup)
        return repo

    def test_first_release(self):
        repo = self.repo()
        repo.commit("Old style subject (#1)")
        self.assertEqual(repo.next()[0], "1.0.0")

    def test_highest_bump_since_the_tag_wins(self):
        repo = self.repo()
        repo.commit("first")
        repo.tag("v1.0.0")
        self.assertEqual(repo.next(), (None, None))
        repo.commit("docs: readme (#2)")
        repo.commit("ci: faster (#3)")
        self.assertEqual(repo.next(), (None, None))
        repo.commit("fix(import): retry a slow share (#4)")
        self.assertEqual(repo.next()[0], "1.0.1")
        repo.commit("feat: updates (#5)")
        version, notes = repo.next()
        self.assertEqual(version, "1.1.0")
        self.assertIn("### New\n\n- updates (#5)", notes)
        self.assertIn("### Fixes\n\n- **import:** retry a slow share (#4)", notes)
        self.assertNotIn("readme", notes)
        repo.tag("v1.1.0")
        repo.commit("feat!: new database layout (#6)")
        version, notes = repo.next()
        self.assertEqual(version, "2.0.0")
        self.assertIn("### Breaking changes\n\n- new database layout (#6)", notes)

    def test_ignores_other_tags(self):
        repo = self.repo()
        repo.commit("first")
        repo.tag("v1.4.2")
        repo.commit("fix: x")
        repo.tag("not-a-version")
        self.assertEqual(repo.next()[0], "1.4.3")


if __name__ == "__main__":
    unittest.main()
