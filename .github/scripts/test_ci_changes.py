import contextlib
import io
import json
import os
from pathlib import Path
import subprocess
import tempfile
import unittest
from unittest.mock import patch

from ci_changes import changed_paths, main, select_jobs


class JobSelectionTests(unittest.TestCase):
    def test_frontend_assets_and_embedded_web_do_not_need_rust(self):
        self.assertEqual(
            select_jobs(["frontend/public/logo.svg", "aether-vscodex/web/package-lock.json"]),
            {"rust": False, "frontend": True},
        )

    def test_frontend_source_contracts_still_run_rust(self):
        for path in ["frontend/src/new.vue", "frontend/vite.config.ts"]:
            with self.subTest(path=path):
                self.assertEqual(select_jobs([path]), {"rust": True, "frontend": True})

    def test_api_documentation_is_part_of_rust_tests(self):
        self.assertEqual(
            select_jobs(["docs/api/provider-interface-definitions.md"]),
            {"rust": True, "frontend": False},
        )
        self.assertEqual(
            select_jobs(["README.md", "docs/operations/deployment.md"]),
            {"rust": False, "frontend": False},
        )

    def test_rust_sources_and_migrations_select_rust(self):
        self.assertEqual(
            select_jobs(["crates/db/migrations/new.sql", "Cargo.lock"]),
            {"rust": True, "frontend": False},
        )

    def test_shared_configuration_and_unknown_paths_select_everything(self):
        for path in [".github/workflows/ci.yml", "Dockerfile.app.local", "new-component/file"]:
            with self.subTest(path=path):
                self.assertEqual(select_jobs([path]), {"rust": True, "frontend": True})

    def test_dispatches_schedules_and_new_branches_select_everything(self):
        for event_name, event in [
            ("workflow_dispatch", {}), ("workflow_call", {}), ("schedule", {}),
            ("push", {"before": "0" * 40}), ("push", {}),
        ]:
            with self.subTest(event_name=event_name, event=event):
                self.assertEqual(
                    select_jobs(changed_paths(event_name, event)),
                    {"rust": True, "frontend": True},
                )

    def test_missing_history_selects_everything(self):
        with patch("ci_changes.subprocess.run", side_effect=OSError("unavailable")):
            with contextlib.redirect_stdout(io.StringIO()):
                paths = changed_paths("push", {"before": "a" * 40})
        self.assertEqual(select_jobs(paths), {"rust": True, "frontend": True})

    def test_reusable_workflow_can_force_full_checks_for_a_push_event(self):
        with tempfile.TemporaryDirectory() as directory:
            event = Path(directory, "event.json")
            event.write_text(json.dumps({"before": "a" * 40}))
            output = Path(directory, "output")
            with patch.dict(os.environ, {
                "GITHUB_EVENT_NAME": "push", "GITHUB_EVENT_PATH": str(event),
                "GITHUB_OUTPUT": str(output), "CI_FORCE_FULL": "true",
            }):
                with patch("ci_changes.changed_paths") as diff:
                    with contextlib.redirect_stdout(io.StringIO()):
                        main()
                    diff.assert_not_called()
            self.assertEqual(output.read_text(), "rust=true\nfrontend=true\n")

    def test_asset_push_cannot_skip_checks_from_an_unfinished_or_failed_base(self):
        for baseline_passed in [False, True]:
            with self.subTest(baseline_passed=baseline_passed):
                with tempfile.TemporaryDirectory() as directory:
                    event = Path(directory, "event.json")
                    event.write_text(json.dumps({"before": "a" * 40}))
                    output = Path(directory, "output")
                    with patch.dict(os.environ, {
                        "GITHUB_EVENT_NAME": "push", "GITHUB_EVENT_PATH": str(event),
                        "GITHUB_OUTPUT": str(output), "CI_FORCE_FULL": "false",
                    }):
                        with patch("ci_changes.changed_paths", return_value=["frontend/public/logo.svg"]):
                            with patch("ci_changes.has_successful_baseline", return_value=baseline_passed):
                                with contextlib.redirect_stdout(io.StringIO()):
                                    main()
                    expected = "rust=false\nfrontend=true\n" if baseline_passed else "rust=true\nfrontend=true\n"
                    self.assertEqual(output.read_text(), expected)

    def test_pull_request_uses_the_tested_merge_against_its_base(self):
        with tempfile.TemporaryDirectory() as directory:
            def git(*args):
                return subprocess.check_output(["git", "-C", directory, *args], stderr=subprocess.DEVNULL).decode().strip()

            git("init", "--initial-branch=main")
            git("config", "user.name", "CI test")
            git("config", "user.email", "ci-test@example.invalid")
            Path(directory, "README.md").write_text("base")
            git("add", ".")
            git("commit", "-m", "base")
            git("checkout", "-b", "feature")
            asset = Path(directory, "frontend/public/logo.svg")
            asset.parent.mkdir(parents=True)
            asset.write_text("asset")
            git("add", ".")
            git("commit", "-m", "asset")
            git("checkout", "main")
            Path(directory, "Cargo.toml").write_text("unrelated base change")
            git("add", ".")
            git("commit", "-m", "advance base")
            base = git("rev-parse", "HEAD")
            git("merge", "--no-ff", "feature", "-m", "test merge")
            with contextlib.chdir(directory):
                paths = changed_paths("pull_request", {"pull_request": {"base": {"sha": base}}})
            self.assertEqual(paths, ["frontend/public/logo.svg"])
            self.assertEqual(select_jobs(paths), {"rust": False, "frontend": True})

    def test_push_includes_earlier_commits_and_both_sides_of_a_rename(self):
        with tempfile.TemporaryDirectory() as directory:
            def git(*args):
                return subprocess.check_output(["git", "-C", directory, *args], stderr=subprocess.DEVNULL).decode().strip()

            git("init")
            git("config", "user.name", "CI test")
            git("config", "user.email", "ci-test@example.invalid")
            source = Path(directory, "crates/example.rs")
            source.parent.mkdir()
            source.write_text("fixture")
            git("add", ".")
            git("commit", "-m", "base")
            base = git("rev-parse", "HEAD")
            destination = Path(directory, "frontend/public/example.rs")
            destination.parent.mkdir(parents=True)
            source.rename(destination)
            git("add", "-A")
            git("commit", "-m", "move")
            Path(directory, "README.md").write_text("docs")
            git("add", ".")
            git("commit", "-m", "docs")
            with contextlib.chdir(directory):
                paths = changed_paths("push", {"before": base})
            self.assertIn("crates/example.rs", paths)
            self.assertIn("frontend/public/example.rs", paths)
            self.assertEqual(select_jobs(paths), {"rust": True, "frontend": True})


if __name__ == "__main__":
    unittest.main()
