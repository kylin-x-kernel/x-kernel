#!/usr/bin/env python3
"""Parse starry-test-harness last_run.json and format failed-case links.

Used by Jenkinsfile PR comments and gitee_check_runs.py Check Run summaries.

Root directory contract
-----------------------
``--root-ws`` / ``root_ws`` may be either:

* the Jenkins workspace root (contains ``.ci/``), or
* the CI root ``<workspace>/.ci`` (what Jenkinsfile puts in the Gitee
  manifest as ``root_ws: ciRootDir()``, matching ``stage_log_path()``).

On-disk lookup tries both layouts. Artifact URLs are always relative to the
workspace root as ``.ci/work/runtime-<arch>/test-harness/...``.
"""

from __future__ import annotations

import argparse
import json
import os
import re
import sys
from pathlib import Path
from typing import Any
from urllib.parse import quote


# Fallback only when callers omit --arch. Prefer Jenkinsfile to pass --arch
# from runtimeTestArchitectures() so this regex is not a second arch list.
RUNTIME_STAGE_RE = re.compile(r"^Runtime Test: kplat-(.+)$")
# starry-test-harness Suite::dir_name(): make target "ci-test" -> logs/ci/
# (see starry-test-harness/src/cli.rs). Artifact URLs still use that dir name.
DEFAULT_SUITE = "ci"
# Make-target / CLI names that map to on-disk log directories.
SUITE_DIR_ALIASES = {
    "ci-test": "ci",
    "ci": "ci",
    "ci-debian": "ci-debian",
    "ci-test-iter": "ci-test-iter",
    "daily-test": "daily",
    "daily": "daily",
    "longevity-test": "longevity",
    "longevity": "longevity",
}
MAX_CASES = 30


def normalize_suite_dir(suite: str) -> str:
    key = (suite or "").strip()
    return SUITE_DIR_ALIASES.get(key, key or DEFAULT_SUITE)


def runtime_arch_from_stage(stage_name: str) -> str | None:
    match = RUNTIME_STAGE_RE.match((stage_name or "").strip())
    if not match:
        return None
    arch = match.group(1).strip()
    return arch or None


def harness_rel_root(arch: str) -> str:
    """Workspace-relative path used in Jenkins artifact URLs."""
    return f".ci/work/runtime-{arch}/test-harness"


def harness_roots(root_ws: str | Path, arch: str) -> list[Path]:
    """Harness checkout roots for workspace or .ci manifest roots."""
    root = Path(root_ws)
    rel = Path("work") / f"runtime-{arch}" / "test-harness"
    return [
        root / rel,  # root_ws == <workspace>/.ci  (Jenkins manifest)
        root / ".ci" / rel,  # root_ws == <workspace>
    ]


def last_run_json_candidates(
    root_ws: str | Path, arch: str, suite: str = DEFAULT_SUITE
) -> list[Path]:
    """Return candidate last_run.json paths for workspace or .ci roots."""
    suite_dir = normalize_suite_dir(suite)
    # Also try the raw suite string in case a future harness changes dir_name.
    suite_dirs = []
    for name in (suite_dir, (suite or "").strip()):
        if name and name not in suite_dirs:
            suite_dirs.append(name)

    candidates: list[Path] = []
    for harness in harness_roots(root_ws, arch):
        for name in suite_dirs:
            candidates.append(harness / "logs" / name / "last_run.json")
    return candidates


def discover_last_run_json(root_ws: str | Path, arch: str) -> Path | None:
    """Fallback: newest logs/*/last_run.json under the harness checkout."""
    found: list[Path] = []
    for harness in harness_roots(root_ws, arch):
        logs_root = harness / "logs"
        if not logs_root.is_dir():
            continue
        found.extend(logs_root.glob("*/last_run.json"))
    if not found:
        return None
    found.sort(key=lambda p: p.stat().st_mtime, reverse=True)
    return found[0]


def last_run_json_path(
    root_ws: str | Path, arch: str, suite: str = DEFAULT_SUITE
) -> Path | None:
    for path in last_run_json_candidates(root_ws, arch, suite=suite):
        if path.is_file():
            return path
    discovered = discover_last_run_json(root_ws, arch)
    if discovered is not None:
        print(
            f"warning: last_run.json not under suite={suite!r}; "
            f"using discovered {discovered}",
            file=sys.stderr,
        )
    return discovered


def load_last_run(path: Path | None) -> dict[str, Any] | None:
    if path is None or not path.is_file():
        return None
    try:
        data = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, UnicodeDecodeError, json.JSONDecodeError):
        return None
    return data if isinstance(data, dict) else None


def failed_cases_from_summary(summary: dict[str, Any]) -> list[dict[str, str]]:
    cases = summary.get("cases") or []
    if not isinstance(cases, list):
        return []

    failed: list[dict[str, str]] = []
    for entry in cases:
        if not isinstance(entry, dict):
            continue
        status = str(entry.get("status") or "").strip().lower()
        if status not in {"failed", "soft_failed"}:
            continue
        name = str(entry.get("name") or "").strip()
        if not name:
            continue
        log_path = str(entry.get("log_path") or "").strip().replace("\\", "/")
        error_summary = str(entry.get("error_summary") or "").strip()
        # Keep markdown compact: first non-empty line, capped.
        if error_summary:
            error_summary = next(
                (line.strip() for line in error_summary.splitlines() if line.strip()),
                "",
            )
            if len(error_summary) > 200:
                error_summary = error_summary[:197] + "..."
        failed.append(
            {
                "name": name,
                "status": status,
                "log_path": log_path,
                "error_summary": error_summary,
            }
        )
    return failed


def artifact_url(build_url: str, rel_path: str) -> str:
    base = (build_url or "").rstrip("/")
    if not base:
        return ""
    if not base.endswith("/artifact"):
        base = f"{base}/artifact"
    parts = [p for p in rel_path.replace("\\", "/").split("/") if p and p != "."]
    encoded = "/".join(quote(p, safe="._-") for p in parts)
    return f"{base}/{encoded}"


def format_failed_cases_markdown(
    cases: list[dict[str, str]],
    *,
    arch: str,
    build_url: str,
    stage_log_url: str = "",
    suite: str = DEFAULT_SUITE,
) -> str:
    if not cases:
        return ""

    harness = harness_rel_root(arch)
    suite_dir = normalize_suite_dir(suite)
    lines = ["**失败 case：**", ""]
    shown = cases[:MAX_CASES]
    for case in shown:
        label = case["name"]
        if case["status"] == "soft_failed":
            label = f"{label} (soft-fail)"
        log_path = case.get("log_path") or ""
        if log_path:
            rel = f"{harness}/{log_path.lstrip('/')}"
            url = artifact_url(build_url, rel)
            if url:
                lines.append(f"- [{label}]({url})")
            else:
                lines.append(f"- `{label}` — `{rel}`")
        else:
            lines.append(f"- `{label}`")
        if case.get("error_summary"):
            lines.append(f"  - {case['error_summary']}")

    omitted = len(cases) - len(shown)
    if omitted > 0:
        lines.append(f"- …另有 {omitted} 个失败 case 未列出")

    summary_rel = f"{harness}/logs/{suite_dir}/last_run.json"
    summary_url = artifact_url(build_url, summary_rel)
    link_bits = []
    if summary_url:
        link_bits.append(f"[last_run.json]({summary_url})")
    if stage_log_url:
        link_bits.append(f"[阶段日志]({stage_log_url})")
    if link_bits:
        lines.append("")
        lines.append(" | ".join(link_bits))

    return "\n".join(lines)


def collect_runtime_failed_case_markdown(
    stage_name: str,
    *,
    root_ws: str,
    build_url: str,
    stage_log_url: str = "",
    suite: str = DEFAULT_SUITE,
    arch: str | None = None,
) -> str:
    resolved_arch = (arch or "").strip() or runtime_arch_from_stage(stage_name)
    if not resolved_arch or not root_ws:
        return ""
    summary_path = last_run_json_path(root_ws, resolved_arch, suite=suite)
    summary = load_last_run(summary_path)
    if not summary:
        return ""
    cases = failed_cases_from_summary(summary)
    if not cases:
        return ""
    # Use the on-disk suite directory (e.g. "ci") for artifact links.
    suite_dir = summary_path.parent.name if summary_path is not None else normalize_suite_dir(suite)
    return format_failed_cases_markdown(
        cases,
        arch=resolved_arch,
        build_url=build_url,
        stage_log_url=stage_log_url,
        suite=suite_dir,
    )


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--root-ws",
        required=True,
        help="Jenkins workspace root, or <workspace>/.ci (manifest root_ws)",
    )
    parser.add_argument(
        "--arch",
        help="Runtime arch from Jenkinsfile.runtimeTestArchitectures(); preferred over stage-name parsing",
    )
    parser.add_argument("--stage-name", help="Full stage name, e.g. Runtime Test: kplat-riscv64")
    parser.add_argument("--build-url", default=os.environ.get("BUILD_URL", ""))
    parser.add_argument("--stage-log-url", default="")
    parser.add_argument("--suite", default=DEFAULT_SUITE)
    parser.add_argument(
        "--format",
        choices=("markdown", "json"),
        default="markdown",
        help="Output format",
    )
    args = parser.parse_args(argv)

    arch = (args.arch or "").strip()
    if not arch and args.stage_name:
        arch = runtime_arch_from_stage(args.stage_name) or ""
        if arch:
            print(
                "warning: --arch omitted; inferred from --stage-name "
                f"({arch!r}). Prefer passing --arch from "
                "Jenkinsfile.runtimeTestArchitectures().",
                file=sys.stderr,
            )
    if not arch:
        print("error: provide --arch or a Runtime Test --stage-name", file=sys.stderr)
        return 2

    summary_path = last_run_json_path(args.root_ws, arch, suite=args.suite)
    summary = load_last_run(summary_path)
    if not summary:
        return 1
    cases = failed_cases_from_summary(summary)
    if args.format == "json":
        json.dump(cases, sys.stdout, ensure_ascii=False, indent=2)
        sys.stdout.write("\n")
        return 0 if cases else 1

    suite_dir = summary_path.parent.name if summary_path is not None else normalize_suite_dir(args.suite)
    text = format_failed_cases_markdown(
        cases,
        arch=arch,
        build_url=args.build_url,
        stage_log_url=args.stage_log_url,
        suite=suite_dir,
    )
    if not text:
        return 1
    sys.stdout.write(text + "\n")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
