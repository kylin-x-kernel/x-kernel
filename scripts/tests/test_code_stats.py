#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
# See LICENSES for license details.

"""Run with python3 -m unittest discover -s scripts/tests -p 'test_code_stats.py'."""

import importlib.util
import json
import shutil
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch


SCRIPT = Path(__file__).resolve().parents[1] / "code_stats.py"
SPEC = importlib.util.spec_from_file_location("code_stats", SCRIPT)
stats = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(stats)


class ClassificationTests(unittest.TestCase):
    def test_exclusive_categories_and_component_boundaries(self):
        cases = {
            "core/demo/src/lib.rs": "kernel",
            "core/demo/ut-tests/case.rs": "tests",
            "core/demo/tests/host/case.rs": "tests",
            "scripts/tests/test_stats.py": "tests",
            "third_party/demo/tests/case.rs": "tests",
            "io/demo/abi-tests/check/src/main.rs": "tests",
            "task/demo/benches/host/ownership.rs": "tests",
            "fuzz/fuzz_targets/example.rs": "tests",
            "core/demo/src/latest.rs": "kernel",
            "core/demo/src/tests_support.rs": "kernel",
            "xtask/xkmake/src/main.rs": "tools",
            "core/demo/build.rs": "tools",
            "third_party/demo/build.rs": "tools",
            "third_party/demo/tests/build.rs": "tests",
            "linker.lds.S": "kernel",
            "unregistered/linker.lds.S": "other_source",
            "third_party/demo/src/lib.rs": "third_party",
            "uapps/demo/src/main.rs": "applications",
            "tee_apps/demo/src/main.rs": "applications",
            "unregistered/lib.rs": "other_source",
        }
        for path, category in cases.items():
            with self.subTest(path=path):
                self.assertEqual(stats.classify(path, "Rust"), category)
        self.assertEqual(stats.classify("core/demo/tests/README.md", "Markdown"), "non_source")
        self.assertEqual(stats.classify("core/demo/Cargo.toml", "TOML"), "non_source")
        self.assertEqual(stats.classify("Makefile", "Makefile"), "tools")

    def test_legacy_scripts_do_not_inflate_kernel_code(self):
        for path in stats.TEST_FILES:
            self.assertEqual(stats.classify(path, "Shell"), "tests")
        self.assertEqual(stats.classify("entry/src/init.sh", "Shell"), "applications")
        self.assertEqual(stats.classify("tee/tee_kernel/src/lib.rs", "Rust"), "kernel")

    def test_documentation_sources_keep_explicit_fallback(self):
        cases = [
            ("docs/experiments/sample/probe.c", "C", "other_source"),
            ("docs/ai/review/module-docs/export_checkpoints.py", "Python", "other_source"),
            ("docs/experiments/sample/README.md", "Markdown", "non_source"),
            ("docs/experiments/sample/tests/probe.c", "C", "tests"),
        ]
        for path, language, category in cases:
            with self.subTest(path=path):
                self.assertEqual(stats.classify(path, language), category)

    def test_text_points_to_rules_without_a_second_test_directory_list(self):
        with patch.object(stats, "PATH_RULES", (("tests", {"roots": {"custom-cases"}}),)):
            output = stats.render_text({
                **stats.summarize([]), "commit": "fixture", "tokei": "fixture",
                "skipped": [], "tracked_worktree_dirty": False,
            })
        self.assertIn("rules object in --format json output", output)
        for directory in ("ut-tests/", "tests/", "abi-tests/", "benches/", "fuzz/"):
            self.assertNotIn(directory, output)

    def test_rules_snapshot_records_matching_and_precedence(self):
        snapshot = json.loads(json.dumps(stats.classification_rules()))
        self.assertEqual(snapshot["source_languages"], sorted(stats.SOURCE_LANGUAGES))
        self.assertEqual(snapshot["non_source_category"], "non_source")
        self.assertEqual(snapshot["fallback_category"], "other_source")
        self.assertEqual([rule["category"] for rule in snapshot["path_rules"]],
                         ["tests", "tools", "third_party", "applications", "kernel"])
        rules = {rule["category"]: rule["match"] for rule in snapshot["path_rules"]}
        self.assertEqual(rules["tools"]["filenames"], ["build.rs"])
        self.assertEqual(rules["kernel"]["files"], ["linker.lds.S"])
        self.assertEqual(rules["tests"]["roots"], ["fuzz"])
        # A precedence change must affect both classification and the snapshot.
        with patch.object(stats, "PATH_RULES", tuple(reversed(stats.PATH_RULES))):
            self.assertEqual(stats.classify("third_party/demo/build.rs", "Rust"), "third_party")
            self.assertNotEqual(stats.classification_rules(), snapshot)
        # A changed filename matcher must not leave the exported rules stale.
        with patch.object(stats, "PATH_RULES", (("tools", {"filenames": {"custom.rs"}}),)):
            self.assertEqual(stats.classify("elsewhere/custom.rs", "Rust"), "tools")
            self.assertEqual(stats.classification_rules()["path_rules"], [
                {"category": "tools", "match": {"filenames": ["custom.rs"]}},
            ])

    def test_unmerged_index_stages_fail_before_counting(self):
        for stage in (1, 2, 3):
            record = f"100644 {'a' * 40} {stage}\tconflicted.rs\0".encode()
            with self.subTest(stage=stage), patch.object(stats, "run_command", return_value=record):
                with self.assertRaisesRegex(RuntimeError, "Resolve the index conflict.*conflicted.rs"):
                    stats.tracked_files(Path("."))

    def test_embedded_counts_are_added_once(self):
        raw = {"code": 2, "comments": 1, "blanks": 0, "blobs": {
            "Markdown": {"code": 0, "comments": 3, "blanks": 1, "blobs": {
                "Rust": {"code": 1, "comments": 0, "blanks": 0, "blobs": {}}
            }}
        }}
        self.assertEqual(stats.line_counts(raw), {"code": 3, "comments": 4, "blanks": 1, "lines": 8})

    def test_non_source_is_not_in_source_total(self):
        records = [
            {"category": category, "language": language, "files": 1,
             "code": 2, "comments": 1, "blanks": 0, "lines": 3}
            for category, language in [("kernel", "Rust"), ("tests", "Rust"), ("non_source", "TOML")]
        ]
        result = stats.summarize(records)
        self.assertEqual(result["source_total"]["code"], 4)
        self.assertEqual(result["recognized_total"]["code"], 6)
        self.assertEqual(result["source_total"]["files"], 2)

    def test_empty_scan_does_not_default_to_current_directory(self):
        with patch.object(stats, "run_command") as command:
            self.assertEqual(stats.scan(Path("."), [], "tokei"), ([], []))
            command.assert_not_called()

    def test_batches_cover_each_file_once(self):
        count = stats.FILE_BATCH_SIZE * 2 + 1
        files = [f"file{i:06}.rs" for i in range(count)]

        def output(args, root):
            paths = args[args.index("--") + 1:]
            return json.dumps({"Rust": {"reports": [
                {"name": path, "stats": {"code": 1, "comments": 0, "blanks": 0}}
                for path in paths
            ]}}).encode()

        with patch.object(stats, "run_command", side_effect=output) as command:
            records, skipped = stats.scan(Path("."), files, "tokei")
            batches = [call.args[0][call.args[0].index("--") + 1:]
                       for call in command.call_args_list]
        self.assertEqual([len(batch) for batch in batches], [stats.FILE_BATCH_SIZE] * 2 + [1])
        self.assertEqual([path for batch in batches for path in batch], files)
        self.assertEqual([record["path"] for record in records], files)
        self.assertEqual(skipped, [])
        self.assertEqual(stats.summarize(records)["source_total"]["code"], count)

    def test_unexpected_reports_and_inaccurate_counts_fail(self):
        duplicate = {"name": "inside.rs", "stats": {"code": 1, "comments": 0, "blanks": 0}}
        for data in [
            {"Total": {"inaccurate": True}},
            {"Rust": {"inaccurate": True, "reports": []}},
            {"Rust": {"reports": [{"name": "outside.rs"}]}},
            {"Rust": {"reports": [duplicate, duplicate]}},
        ]:
            with patch.object(stats, "run_command", return_value=json.dumps(data).encode()):
                with self.assertRaises(RuntimeError):
                    stats.scan(Path("."), ["inside.rs"], "tokei")

    def test_invalid_line_counts_fail(self):
        for value in (-1, True, "2"):
            with self.assertRaises(ValueError):
                stats.line_counts({"code": value, "comments": 0, "blanks": 0})

    def test_cli_missing_tool_fails_without_partial_report(self):
        result = subprocess.run(
            [sys.executable, str(SCRIPT), "--tokei", "/nonexistent/code-stats-tokei"],
            stdout=subprocess.PIPE, stderr=subprocess.PIPE,
        )
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(result.stdout, b"")
        self.assertIn(b"code-stats:", result.stderr)


@unittest.skipUnless(shutil.which("git"), "requires Git")
class GitRootTests(unittest.TestCase):
    def test_cli_rejects_subdirectory_root_before_running_tokei(self):
        with tempfile.TemporaryDirectory(prefix="code-stats-root-") as directory:
            root = Path(directory).resolve()
            subprocess.run(["git", "init", "-q", str(root)], check=True)
            nested = root / "nested"
            nested.mkdir()
            result = subprocess.run([
                sys.executable, str(SCRIPT), "--root", str(nested),
                "--tokei", "/nonexistent/code-stats-tokei",
            ], stdout=subprocess.PIPE, stderr=subprocess.PIPE)
            self.assertEqual(result.returncode, 1)
            self.assertEqual(result.stdout, b"")
            self.assertIn(b"--root must be the Git repository root", result.stderr)


@unittest.skipUnless(shutil.which("tokei") and shutil.which("git"), "requires Git and tokei with JSON support")
class TokeiIntegrationTests(unittest.TestCase):
    def test_cli_revision_formats_and_dirty_state(self):
        with tempfile.TemporaryDirectory(prefix="code-stats-cli-") as directory:
            root = Path(directory).resolve()
            subprocess.run(["git", "init", "-q", str(root)], check=True)
            source = root / "main.rs"
            source.write_text("fn main() {}\n")
            subprocess.run(["git", "-C", str(root), "add", "main.rs"], check=True)
            subprocess.run([
                "git", "-C", str(root), "-c", "core.hooksPath=/dev/null",
                "-c", "user.name=Fixture", "-c", "user.email=fixture@example.invalid",
                "-c", "commit.gpgsign=false", "commit", "-qm", "fixture",
            ], check=True)
            revision = subprocess.check_output([
                "git", "-C", str(root), "rev-parse", "HEAD",
            ]).decode().strip()
            args = [sys.executable, str(SCRIPT), "--root", str(root)]
            clean = json.loads(subprocess.check_output(args + ["--format", "json"], cwd=root.parent))
            self.assertEqual(clean["commit"], revision)
            self.assertFalse(clean["tracked_worktree_dirty"])
            self.assertEqual(clean["schema_version"], 2)
            self.assertEqual(clean["rules"], stats.classification_rules())
            self.assertEqual(clean["source_total"]["code"], 1)
            source.write_text("fn main() {}\nfn extra() {}\n")
            dirty = json.loads(subprocess.check_output(args + ["--format", "json"], cwd=root.parent))
            self.assertTrue(dirty["tracked_worktree_dirty"])
            self.assertEqual(dirty["source_total"]["code"], 2)
            text = subprocess.check_output(args, cwd=root.parent).decode()
            self.assertEqual(text.rstrip(), stats.render_text(dirty))

    def test_real_tracked_tree_and_reconciled_totals(self):
        with tempfile.TemporaryDirectory(prefix="code stats src ") as directory:
            root = Path(directory).resolve()
            subprocess.run(["git", "init", "-q", str(root)], check=True)
            fixtures = {
                "core/demo/src/lib.rs": "//! Documentation\n//! ```rust\n//! let a = 1;\n//! ```\n\npub fn demo() {}\n",
                "core/demo/tests/space name.rs": "// test\n#[test]\nfn case() {}\n",
                "core/demo/ut-tests/case.rs": "#[unittest::def_test]\nfn guest() {}\n",
                "scripts/helper.py": "# helper\nprint('ok')\n",
                "third_party/demo/src/lib.rs": "pub fn dependency() {}\n",
                "README.md": "# Guide\n\n```rust\nfn example() {}\n```\n",
                "Cargo.toml": "[workspace]\nmembers = []\n",
                "deleted.rs": "fn gone() {}\n",
                "unrecognized.unknown_extension": "not a source language\n",
                ".hidden.rs": "fn hidden() {}\n",
                "target/tracked.rs": "fn explicitly_tracked() {}\n",
            }
            for name, content in fixtures.items():
                path = root / name
                path.parent.mkdir(parents=True, exist_ok=True)
                path.write_text(content)
            (root / "alias.rs").symlink_to("core/demo/src/lib.rs")
            subprocess.run(["git", "-C", str(root), "add", "--all"], check=True)
            (root / "deleted.rs").unlink()
            (root / "untracked.rs").write_text("fn untracked() {}\n")
            # Explicit tracked input must not depend on ignore rules or cwd names.
            (root / ".gitignore").write_text("target/\n.hidden.rs\n")
            files, skipped = stats.tracked_files(root)
            self.assertNotIn("untracked.rs", files)
            self.assertEqual({item["path"] for item in skipped}, {"alias.rs", "deleted.rs"})
            records, unknown = stats.scan(root, files, "tokei")
            self.assertEqual([item["path"] for item in unknown], ["unrecognized.unknown_extension"])
            self.assertEqual(len(records), len(files) - 1)
            for record in records:
                self.assertEqual(record["lines"], len(fixtures[record["path"]].splitlines()), record["path"])
            report = stats.summarize(records)
            for metric in stats.METRICS:
                self.assertEqual(report["recognized_total"][metric], sum(record[metric] for record in records))
                self.assertEqual(report["source_total"][metric], sum(
                    counts[metric] for name, counts in report["categories"].items() if name != "non_source"
                ))
            output = stats.render_text({
                **report, "commit": "fixture", "tokei": "fixture", "skipped": skipped + unknown,
                "tracked_worktree_dirty": True,
            })
            self.assertIn("ALL SOURCE", output)


if __name__ == "__main__":
    unittest.main()
