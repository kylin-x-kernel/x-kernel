#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
# See LICENSES for license details.

"""Classify Git-tracked files and summarize tokei's line counts."""

from __future__ import annotations

import argparse
import json
import subprocess
import sys
from collections import Counter
from pathlib import Path, PurePosixPath


# Names are tokei JSON language names, not filename suffix guesses. Unknown
# languages remain visible in non_source instead of inflating kernel LOC.
SOURCE_LANGUAGES = {
    "Rust", "C", "C Header", "C++", "C++ Header", "Assembly",
    "GNU Style Assembly", "Python", "BASH", "Shell", "Zsh", "C Shell",
    "Makefile", "CMake", "Dockerfile", "Groovy", "JavaScript", "TypeScript",
    "Go", "Lua", "Perl", "AWK",
}
KERNEL_ROOTS = {
    "api", "arch", "boot", "core", "drivers", "entry", "fs", "io", "mm",
    "net", "platforms", "posix", "process", "sync", "task", "tee", "util", "virt",
}
TOOL_ROOTS = {"scripts", "tools", "xtask", ".githooks", ".github", ".ci"}
TOOL_FILES = {"Makefile", "Jenkinsfile"}
TEST_DIRS = {"ut-tests", "tests", "abi-tests", "benches"}
# Legacy harnesses outside the standard test directories are explicit, not
# inferred from names such as test_*.rs that may also name production code.
TEST_FILES = {
    "tee/tee_kernel/scripts/tee_test_multi_process_with_diff_objid.sh",
    "tee/tee_kernel/scripts/tee_test_multi_process_with_same_objid.sh",
    "tee/tee_kernel/scripts/tee_test_storage_concurrency.sh",
}
APPLICATION_ROOTS = {"uapps", "tee_apps"}
APPLICATION_FILES = {"entry/src/init.sh"}
# First matching path rule wins after the source-language gate. This same
# ordered table is serialized into the report so precedence is auditable.
# docs/ intentionally has no blanket rule: probes, examples and review helpers
# fall back to other_source unless a more specific rule (such as tests/) matches.
PATH_RULES = (
    ("tests", {"directories": TEST_DIRS, "roots": {"fuzz"}, "files": TEST_FILES}),
    ("tools", {"roots": TOOL_ROOTS, "files": TOOL_FILES, "filenames": {"build.rs"}}),
    ("third_party", {"roots": {"third_party"}}),
    ("applications", {"roots": APPLICATION_ROOTS, "files": APPLICATION_FILES}),
    ("kernel", {"roots": KERNEL_ROOTS, "files": {"linker.lds.S"}}),
)
NON_SOURCE_CATEGORY = "non_source"
FALLBACK_CATEGORY = "other_source"
FILE_BATCH_SIZE = 128
CATEGORIES = (
    "kernel", "tests", "tools", "third_party", "applications", "other_source",
    "non_source",
)
METRICS = ("files", "code", "comments", "blanks", "lines")


def run_command(args: list[str], root: Path) -> bytes:
    result = subprocess.run(args, cwd=root, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
    if result.returncode:
        raise RuntimeError(
            f"{args[0]} failed ({result.returncode}): "
            + result.stderr.decode(errors="replace").strip()
        )
    return result.stdout


def tracked_files(root: Path) -> tuple[list[str], list[dict]]:
    files, skipped = [], []
    records = run_command(["git", "ls-files", "--stage", "-z"], root).split(b"\0")
    for record in filter(None, records):
        info, raw_path = record.split(b"\t", 1)
        mode, _, stage = info.split()
        path = raw_path.decode("utf-8")
        if stage != b"0":
            raise RuntimeError(f"Resolve the index conflict before counting: {path}")
        if mode not in (b"100644", b"100755"):
            skipped.append({"path": path, "reason": "symlink or submodule"})
        elif (root / path).resolve() != root / path or not (root / path).is_file():
            skipped.append({"path": path, "reason": "missing or non-regular worktree file"})
        else:
            files.append(path)
    return sorted(files), sorted(skipped, key=lambda item: item["path"])


def classify(path: str, language: str) -> str:
    parts = PurePosixPath(path).parts
    if language not in SOURCE_LANGUAGES:
        return NON_SOURCE_CATEGORY
    # Directory components, not substrings: latest.rs is not a test file.
    for category, match in PATH_RULES:
        if (any(part in match.get("directories", ()) for part in parts[:-1])
                or parts[0] in match.get("roots", ())
                or path in match.get("files", ())
                or parts[-1] in match.get("filenames", ())):
            return category
    return FALLBACK_CATEGORY


def classification_rules() -> dict:
    return {
        "source_languages": sorted(SOURCE_LANGUAGES),
        "non_source_category": NON_SOURCE_CATEGORY,
        "path_rules": [
            {"category": category, "match": {key: sorted(values) for key, values in match.items()}}
            for category, match in PATH_RULES
        ],
        "fallback_category": FALLBACK_CATEGORY,
    }


def line_counts(stats: dict) -> dict[str, int]:
    counts = {key: stats[key] for key in ("code", "comments", "blanks")}
    if any(type(value) is not int or value < 0 for value in counts.values()):
        raise ValueError("Invalid tokei line counts")
    # Each report's blobs are its embedded languages. Top-level children and
    # Total repeat these records; never add them a second time.
    for blob in stats.get("blobs", {}).values():
        child = line_counts(blob)
        for key in counts:
            counts[key] += child[key]
    counts["lines"] = sum(counts.values())
    return counts


def scan(root: Path, files: list[str], tokei: str) -> tuple[list[dict], list[dict]]:
    results = {}
    # Bound argv length; a file is passed exactly once and directories are never
    # scanned, so ignored build artifacts cannot sneak into the totals.
    for offset in range(0, len(files), FILE_BATCH_SIZE):
        batch = files[offset:offset + FILE_BATCH_SIZE]
        data = json.loads(run_command(
            [tokei, "--files", "--output", "json", "--hidden", "--no-ignore", "--", *batch],
            root,
        ))
        if data.get("Total", {}).get("inaccurate"):
            raise RuntimeError("tokei reported inaccurate total statistics")
        expected = set(batch)
        for language, summary in data.items():
            if language == "Total":
                continue
            if summary.get("inaccurate"):
                raise RuntimeError(f"tokei reported inaccurate statistics for {language}")
            for report in summary["reports"]:
                path = PurePosixPath(report["name"]).as_posix()
                if path not in expected or path in results:
                    raise RuntimeError(f"Unexpected or duplicate tokei file: {path}")
                results[path] = {
                    "path": path, "language": language, "category": classify(path, language),
                    "files": 1, **line_counts(report["stats"]),
                }
    skipped = [{"path": path, "reason": "not recognized by tokei"}
               for path in files if path not in results]
    return [results[path] for path in sorted(results)], skipped


def summarize(records: list[dict]) -> dict:
    categories = {category: Counter({key: 0 for key in METRICS}) for category in CATEGORIES}
    languages = {}
    for record in records:
        counts = {key: record[key] for key in METRICS}
        categories[record["category"]].update(counts)
        languages.setdefault(record["category"], {}).setdefault(record["language"], Counter()).update(counts)
    source_total = Counter({key: 0 for key in METRICS})
    for category, counts in categories.items():
        if category != "non_source":
            source_total.update(counts)
    recognized_total = source_total.copy()
    recognized_total.update(categories["non_source"])
    return {
        "source_total": dict(source_total), "recognized_total": dict(recognized_total),
        "categories": categories, "languages": languages,
    }


def render_text(report: dict) -> str:
    lines = [f"Commit: {report['commit']}", f"Tool: {report['tokei']}",
             "Scope: tracked working-tree files (not a committed-tree snapshot)",
             f"Tracked worktree changes: {report['tracked_worktree_dirty']}",
             "", f"{'Category / language':<32} {'Files':>7} {'Code':>10} {'Comments':>10} {'Blank':>10} {'Lines':>10}"]

    def row(label: str, counts: dict) -> None:
        lines.append(f"{label:<32} " + " ".join(
            f"{counts[key]:>{7 if key == 'files' else 10},}" for key in METRICS
        ))

    row("ALL SOURCE", report["source_total"])
    for category in CATEGORIES:
        row(category, report["categories"][category])
        for language, counts in sorted(report["languages"].get(category, {}).items()):
            row("  " + language, counts)
    row("ALL RECOGNIZED (incl. non_source)", report["recognized_total"])
    lines += ["", f"Uncounted files: {len(report['skipped'])} (paths and reasons in JSON)",
              "Complete classification rules: see the rules object in --format json output.",
              "Embedded language counts stay with their containing file; non_source is excluded from ALL SOURCE."]
    return "\n".join(lines)


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--root", type=Path, default=Path(__file__).resolve().parents[1])
    parser.add_argument("--tokei", default="tokei", help="tokei executable with JSON support")
    parser.add_argument("--format", choices=("text", "json"), default="text")
    args = parser.parse_args()
    try:
        root = args.root.resolve()
        actual_root = run_command(["git", "rev-parse", "--show-toplevel"], root).decode().strip()
        if Path(actual_root).resolve() != root:
            raise RuntimeError("--root must be the Git repository root")
        version = run_command([args.tokei, "--version"], root).decode().strip()
        files, skipped = tracked_files(root)
        records, unrecognized = scan(root, files, args.tokei)
        report = {
            "schema_version": 2,
            "commit": run_command(["git", "rev-parse", "HEAD"], root).decode().strip(),
            "scope": "tracked working-tree files", "tokei": version,
            "tracked_worktree_dirty": bool(run_command(
                ["git", "status", "--porcelain=v1", "--untracked-files=no", "-z"], root
            )),
            "rules": classification_rules(),
            **summarize(records), "files": records,
            "skipped": sorted(skipped + unrecognized, key=lambda item: item["path"]),
        }
        print(json.dumps(report, ensure_ascii=False, indent=2, sort_keys=True)
              if args.format == "json" else render_text(report))
        return 0
    except (OSError, UnicodeError, ValueError, KeyError, TypeError, RuntimeError) as error:
        print(f"code-stats: {error}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
