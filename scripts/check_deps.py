#!/usr/bin/env python3
"""
Scan all crates in the workspace for unused dependencies.
For each crate, parse [dependencies] and [dev-dependencies] from Cargo.toml,
then search all .rs files for actual usage. Unused deps are optionally removed.

Usage:
    # Check only (default) — report unused deps, exit 1 if any found
    python3 scripts/check_deps.py

    # Actually remove unused deps from Cargo.toml
    python3 scripts/check_deps.py --fix

    # Scan a specific crate only
    python3 scripts/check_deps.py --crate core/kcore

    # Verbose output
    python3 scripts/check_deps.py -v
"""

from __future__ import annotations

import argparse
import json
import os
import re
import subprocess
import sys
from dataclasses import dataclass, field
from pathlib import Path


# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------

def find_project_root() -> Path:
    """Find the workspace root by looking for the root Cargo.toml with [workspace]."""
    script_dir = Path(__file__).resolve().parent
    for parent in [script_dir] + list(script_dir.parents):
        cargo_toml = parent / "Cargo.toml"
        if cargo_toml.exists():
            content = cargo_toml.read_text()
            if "[workspace]" in content:
                return parent
    return Path.cwd()


def find_all_crates(root: Path) -> list[Path]:
    """Find all Cargo.toml files that define a [package] (skip workspace root)."""
    crates: list[Path] = []
    for dirpath, _, filenames in os.walk(root):
        parts = Path(dirpath).parts
        if "target" in parts or ".git" in parts:
            continue
        if "Cargo.toml" in filenames:
            cargo_path = Path(dirpath) / "Cargo.toml"
            content = cargo_path.read_text()
            if "[package]" in content:
                crates.append(cargo_path)
    crates.sort()
    return crates


def crate_name_to_rust_ident(crate_name: str) -> str:
    """Convert a crate name (hyphens) to the Rust identifier used in code (underscores)."""
    return crate_name.replace("-", "_")


# ---------------------------------------------------------------------------
# Parsing Cargo.toml (lightweight, no external deps)
# ---------------------------------------------------------------------------

# Pattern for section headers:
#   [dependencies]                    -> group(1)="dependencies"
#   [dev-dependencies]                -> group(1)="dev-dependencies"
#   [dependencies.smoltcp]            -> group(1)="dependencies", group(2)="smoltcp"
#   [target.'cfg(...)'.dependencies]  -> special case
RE_SECTION = re.compile(
    r"^\[" r"(dev-)?dependencies" r"(?:\.([a-zA-Z_][a-zA-Z0-9_-]*))?" r"\]$"
)
RE_TARGET_SECTION = re.compile(
    r"^\[target\.'[^']*'\.(dev-)?dependencies\]$"
)


@dataclass
class Dependency:
    name: str          # Key in Cargo.toml (the crate identifier used in Rust code)
    package_name: str  # Actual crate name (from `package = ...` if renamed)
    section: str       # "dependencies" or "dev-dependencies"
    optional: bool     # True if optional = true (feature-gated dep, skip from removal)
    line_start: int    # 0-based line number where this dep starts
    line_end: int      # 0-based line number where this dep ends


@dataclass
class ParsedToml:
    path: Path
    lines: list[str]
    deps: list[Dependency] = field(default_factory=list)
    features_text: str = ""  # Raw text of the [features] section for reference scanning


@dataclass
class ResolvedDependency:
    package_name: str
    rust_name: str
    sections: set[str]


def parse_cargo_toml(path: Path) -> ParsedToml:
    """Parse a Cargo.toml and extract dependency info with line ranges.

    Handles three dependency declaration formats:
    1. Inline:  `name = "version"` or `name = { workspace = true }`
    2. Multi-line table:
           [dependencies.name]
           workspace = true
           features = [...]
    """
    raw_text = path.read_text()
    lines = raw_text.splitlines()
    result = ParsedToml(path=path, lines=lines)

    # Extract [features] section text for reference scanning
    # Match everything from [features] until the next section header [...]
    features_match = re.search(
        r"^\[features\]\s*\n(.*?)(?=^\[)", raw_text, re.MULTILINE | re.DOTALL
    )
    if features_match:
        result.features_text = features_match.group(1)

    # State machine for tracking what we're inside
    # None = not in a dep section
    # ("dependencies", dep_name, line_start) or ("dev-dependencies", dep_name, line_start)
    table_dep: tuple | None = None  # For [dependencies.xxx] multi-line table

    current_dep_section = ""  # "dependencies" or "dev-dependencies" or ""
    i = 0
    while i < len(lines):
        stripped = lines[i].strip()

        # --- Check for section headers ---
        # [dependencies.name] multi-line table
        m = RE_SECTION.match(stripped)
        if m:
            is_dev = m.group(1) is not None
            table_name = m.group(2)  # None for plain [dependencies]
            current_dep_section = "dev-dependencies" if is_dev else "dependencies"

            if table_name is not None:
                # Multi-line table: [dependencies.name]
                # The dep starts at this line, collect until next section
                table_dep = (current_dep_section, table_name, i)
            else:
                # Plain [dependencies] section
                table_dep = None
            i += 1
            continue

        # [target.'cfg(...)'.dependencies]
        m_target = RE_TARGET_SECTION.match(stripped)
        if m_target:
            is_dev = m_target.group(1) is not None
            current_dep_section = "dev-dependencies" if is_dev else "dependencies"
            table_dep = None
            i += 1
            continue

        # Any other section header resets dep context
        if stripped.startswith("["):
            current_dep_section = ""
            table_dep = None
            i += 1
            continue

        # --- Inside a multi-line table [dependencies.name] ---
        if table_dep is not None:
            # We're collecting key=value pairs for this dep.
            # A blank line or next section ends it (next section handled above).
            # We just need to track where it ends.
            # Nothing to do per-line; we'll finalize when we leave.
            if not stripped or stripped.startswith("#"):
                # Blank/comment — check if this ends the table
                # Peek ahead: if next non-blank line is a section, the table ends here
                # For now, keep going; we finalize on section change
                i += 1
                continue
            # It's a key=value inside the table, keep going
            i += 1
            continue

        # --- Inside a plain [dependencies] section ---
        if current_dep_section not in ("dependencies", "dev-dependencies"):
            i += 1
            continue

        # Skip blank lines and comments
        if not stripped or stripped.startswith("#"):
            i += 1
            continue

        # Parse inline dep: name = ... or name.attribute = ...
        # TOML dotted keys like "kfeat.workspace = true" mean dep "kfeat" with attr "workspace"
        dep_match = re.match(r"^([a-zA-Z_][a-zA-Z0-9_-]*)(?:\.[a-zA-Z_][a-zA-Z0-9_]*)*\s*=", stripped)
        if not dep_match:
            i += 1
            continue

        dep_key = dep_match.group(1)
        line_start = i
        line_end = i

        rest = stripped[stripped.index("=") + 1:].strip()

        if rest.startswith("{"):
            # Inline table — may span multiple lines
            brace_count = rest.count("{") - rest.count("}")
            j = i
            while brace_count > 0 and j + 1 < len(lines):
                j += 1
                brace_count += lines[j].count("{") - lines[j].count("}")
            line_end = j

        # Extract package name if renamed, and check optional
        all_lines_text = "\n".join(lines[line_start : line_end + 1])
        pkg_match = re.search(r'package\s*=\s*"([^"]+)"', all_lines_text)
        package_name = pkg_match.group(1) if pkg_match else dep_key
        is_optional = bool(re.search(r'optional\s*=\s*true', all_lines_text))

        result.deps.append(Dependency(
            name=dep_key,
            package_name=package_name,
            section=current_dep_section,
            optional=is_optional,
            line_start=line_start,
            line_end=line_end,
        ))

        i = line_end + 1
        continue

    # Finalize any pending multi-line table dep
    if table_dep is not None:
        _finalize_table_dep(result, table_dep, len(lines) - 1, lines)

    return result


def resolved_dependencies_from_metadata(
    metadata: dict,
) -> dict[Path, list[ResolvedDependency]]:
    """Extract resolved dependency names from Cargo metadata JSON."""
    package_by_id = {pkg["id"]: pkg for pkg in metadata.get("packages", [])}
    resolved_by_manifest: dict[Path, list[ResolvedDependency]] = {}

    for node in metadata.get("resolve", {}).get("nodes", []):
        pkg = package_by_id.get(node["id"])
        if pkg is None:
            continue

        manifest_path = Path(pkg["manifest_path"])
        resolved_deps: list[ResolvedDependency] = []

        for dep in node.get("deps", []):
            dep_pkg = package_by_id.get(dep["pkg"])
            if dep_pkg is None:
                continue

            sections: set[str] = set()
            for dep_kind in dep.get("dep_kinds", []):
                kind = dep_kind.get("kind")
                if kind == "dev":
                    sections.add("dev-dependencies")
                elif kind in (None, "normal", "build"):
                    sections.add("dependencies")

            # `dep["name"]` is the manifest alias (hyphen-bearing). The actual
            # rust identifier visible to source code is the dependency's lib
            # target name (which may be set via `[lib] name = ...` in the
            # upstream crate — e.g. package `rust-libutee` exposes lib `rust_utee`).
            # Fall back to the dep alias converted to a rust ident if no lib
            # target exists (proc-macro / build-only).
            lib_target_name = next(
                (
                    t["name"]
                    for t in dep_pkg.get("targets", [])
                    if "lib" in t.get("kind", []) or "rlib" in t.get("kind", [])
                ),
                None,
            )
            rust_name = lib_target_name or crate_name_to_rust_ident(dep["name"])

            resolved_deps.append(ResolvedDependency(
                package_name=dep_pkg["name"],
                rust_name=rust_name,
                sections=sections,
            ))

        resolved_by_manifest[manifest_path] = resolved_deps

    return resolved_by_manifest


def run_cargo_metadata(args: list[str], cwd: Path, warn: bool = True) -> dict | None:
    try:
        proc = subprocess.run(
            ["cargo", "metadata", "--format-version=1", *args],
            cwd=cwd,
            check=True,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
        )
    except (OSError, subprocess.CalledProcessError) as exc:
        if warn:
            print(f"Warning: failed to run cargo metadata {' '.join(args)}: {exc}", file=sys.stderr)
        return None

    return json.loads(proc.stdout)


def load_resolved_dependencies(root: Path) -> dict[Path, list[ResolvedDependency]]:
    """Load Cargo's resolved dependency names for workspace package manifests.

    Cargo package names are not always the Rust crate names visible from code.
    For example, the `md-5` package exposes the `md5` library target. The
    resolve graph records the exact name Cargo makes available to each package,
    including explicit dependency renames.
    """
    metadata = run_cargo_metadata(["--locked"], root)
    if metadata is None:
        return {}

    return resolved_dependencies_from_metadata(metadata)


def load_manifest_resolved_dependencies(
    manifest_path: Path,
    warn: bool = False,
) -> list[ResolvedDependency]:
    """Load resolved dependency names for a crate outside the root workspace."""
    metadata = run_cargo_metadata(
        ["--offline", "--manifest-path", str(manifest_path)],
        manifest_path.parent,
        warn=warn,
    )
    if metadata is None:
        return []

    return resolved_dependencies_from_metadata(metadata).get(manifest_path.resolve(), [])


def resolve_rust_name(
    dep: Dependency,
    resolved_deps: list[ResolvedDependency],
) -> str:
    """Return the Rust crate name visible to source code for a dependency."""
    candidates = [
        resolved
        for resolved in resolved_deps
        if resolved.package_name == dep.package_name and dep.section in resolved.sections
    ]
    if len(candidates) == 1:
        return candidates[0].rust_name

    candidates = [
        resolved
        for resolved in resolved_deps
        if resolved.package_name == dep.package_name
    ]
    if len(candidates) == 1:
        return candidates[0].rust_name

    return crate_name_to_rust_ident(dep.name)


def _finalize_table_dep(
    result: ParsedToml,
    table_dep: tuple,
    end_line: int,
    lines: list[str],
) -> None:
    """Convert a [dependencies.name] multi-line table into a Dependency."""
    section, dep_name, start_line = table_dep

    # Find actual end (last non-blank, non-comment line before end_line)
    actual_end = start_line
    for j in range(end_line, start_line, -1):
        s = lines[j].strip()
        if s and not s.startswith("#"):
            actual_end = j
            break

    # Extract package name if renamed, and check optional
    all_lines_text = "\n".join(lines[start_line : actual_end + 1])
    pkg_match = re.search(r'package\s*=\s*"([^"]+)"', all_lines_text)
    package_name = pkg_match.group(1) if pkg_match else dep_name
    is_optional = bool(re.search(r'optional\s*=\s*true', all_lines_text))

    result.deps.append(Dependency(
        name=dep_name,
        package_name=package_name,
        section=section,
        optional=is_optional,
        line_start=start_line,
        line_end=actual_end,
    ))


# ---------------------------------------------------------------------------
# Scanning Rust source files for dependency usage
# ---------------------------------------------------------------------------

_PATTERNS: dict[str, re.Pattern] = {}


def get_usage_pattern(rust_name: str) -> re.Pattern:
    """Build a regex to detect usage of a crate in Rust source."""
    if rust_name in _PATTERNS:
        return _PATTERNS[rust_name]

    # Match various usage patterns:
    #   use <rust_name>:: / pub use <rust_name>:: / pub(crate) use <rust_name>::
    #   extern crate <rust_name>
    #   <rust_name>::  (qualified path)
    #   <rust_name>!   (macro invocation)
    #   #[<rust_name>  (attribute)
    pattern = re.compile(
        r"(?:^(?:pub\s+)?(?:\([^)]*\)\s+)?use\s+.*\b" + re.escape(rust_name) + r"\b)"
        r"|(?:^extern\s+crate\s+" + re.escape(rust_name) + r"\b)"
        r"|(?:\b" + re.escape(rust_name) + r"\s*::)"
        r"|(?:\b" + re.escape(rust_name) + r"\s*!)"
        r"|(?:#\[.*\b" + re.escape(rust_name) + r"\b)",
        re.MULTILINE,
    )
    _PATTERNS[rust_name] = pattern
    return pattern


def collect_rust_sources(crate_dir: Path, section: str) -> list[Path]:
    """Collect .rs files relevant to the dependency section."""
    rust_files: list[Path] = []

    for dirpath, _, filenames in os.walk(crate_dir):
        p = Path(dirpath)
        rel = p.relative_to(crate_dir)
        parts = rel.parts

        if "target" in parts or any(part.startswith(".") for part in parts):
            continue

        for fn in filenames:
            if not fn.endswith(".rs"):
                continue

            file_path = p / fn

            if section == "dev-dependencies":
                top = parts[0] if parts else ""
                if top in ("tests", "examples", "benches", "benchs"):
                    rust_files.append(file_path)
                elif top == "src":
                    # Dev-deps may be used in #[cfg(test)] blocks
                    rust_files.append(file_path)
            else:
                rust_files.append(file_path)

    return rust_files


def is_dep_used(
    dep: Dependency,
    rust_name: str,
    rust_files: list[Path],
    features_text: str = "",
) -> bool:
    """Check if a dependency is used in any of the given Rust source files or [features]."""
    # Check [features] section for "dep_name/..." or "dep:dep_name" references
    if features_text:
        # Match patterns like: "kcpu/fp-simd" or "dep:kcpu" in feature lists
        # Feature references use Cargo dependency keys, while source uses the
        # resolved Rust crate name.
        feature_names = {
            dep.name,
            dep.package_name,
            crate_name_to_rust_ident(dep.name),
            crate_name_to_rust_ident(dep.package_name),
            rust_name,
        }
        for feature_name in feature_names:
            esc = re.escape(feature_name)
            feat_re = r"(?<![\w-])" + esc + r"/"
            feat_re += r"|dep:" + esc + r"(?![\w-])"
            if re.search(feat_re, features_text):
                return True

    # Check Rust source files
    pattern = get_usage_pattern(rust_name)

    for f in rust_files:
        try:
            content = f.read_text()
        except (OSError, UnicodeDecodeError):
            continue
        if pattern.search(content):
            return True

    return False


# ---------------------------------------------------------------------------
# Known false positives: crates used implicitly (proc-macros, etc.)
# ---------------------------------------------------------------------------

IMPLICIT_CRATES = {
    "linkme",
    "inherit-methods-macro",
    "lazy_static",
    "strum",
    "bitflags",
    "cfg-if",
    "scope-local",
    "extern-trait",
    "event-listener",
    "percpu",
    "log",
    "macros",
    "kbuild_config",
    "backtrace",
}

IMPLICIT_DEV_CRATES = {
    "unittest",
}


# ---------------------------------------------------------------------------
# Removing deps from Cargo.toml
# ---------------------------------------------------------------------------

def remove_deps_from_toml(parsed: ParsedToml, unused_deps: list[Dependency]) -> None:
    """Remove unused dependency lines from a parsed Cargo.toml and write back.

    Also removes any now-empty section headers (e.g. [target.'cfg(...)'.dependencies])
    left behind after the removal.
    """
    lines_to_remove: set[int] = set()
    for dep in unused_deps:
        for line_no in range(dep.line_start, dep.line_end + 1):
            lines_to_remove.add(line_no)

    # Build the new lines list (preserving indices for header scan)
    new_lines = [
        line for i, line in enumerate(parsed.lines)
        if i not in lines_to_remove
    ]

    # Remove empty dependency sections — a section header followed by
    # another section header (or end-of-file) with only blank lines between.
    cleaned: list[str] = []
    i = 0
    while i < len(new_lines):
        stripped = new_lines[i].strip()
        # Is this a dep-related section header?
        if re.match(r"^\[.*dependencies", stripped):
            # Look ahead: is the next non-blank, non-comment line another header?
            j = i + 1
            while j < len(new_lines) and (not new_lines[j].strip() or new_lines[j].strip().startswith("#")):
                j += 1
            # If next meaningful line is a section header or EOF, skip this header + trailing blanks
            if j >= len(new_lines) or new_lines[j].strip().startswith("["):
                # Skip the header and trailing blank lines
                i += 1
                while i < len(new_lines) and not new_lines[i].strip():
                    i += 1
                continue
        cleaned.append(new_lines[i])
        i += 1

    parsed.path.write_text("\n".join(cleaned) + "\n")


# ---------------------------------------------------------------------------
# Main
# ---------------------------------------------------------------------------

def main() -> None:
    parser = argparse.ArgumentParser(
        description="Find and optionally remove unused dependencies from Cargo.toml files",
    )
    parser.add_argument("--fix", action="store_true",
                        help="Actually remove unused deps (default: dry-run)")
    parser.add_argument("--crate", type=str, default=None,
                        help="Only scan a specific crate (relative path from root)")
    parser.add_argument("-v", "--verbose", action="store_true",
                        help="Show verbose output including used deps")
    args = parser.parse_args()

    root = find_project_root()
    print(f"Workspace root: {root}\n")

    if args.crate:
        crate_path = root / args.crate / "Cargo.toml"
        if not crate_path.exists():
            print(f"Error: {crate_path} not found", file=sys.stderr)
            sys.exit(1)
        all_crates = [crate_path]
    else:
        all_crates = find_all_crates(root)

    resolved_by_manifest = load_resolved_dependencies(root)

    total_unused = 0
    total_deps = 0
    crates_with_unused: list[str] = []

    for cargo_path in all_crates:
        crate_dir = cargo_path.parent
        rel_path = cargo_path.relative_to(root)

        parsed = parse_cargo_toml(cargo_path)
        if not parsed.deps:
            continue

        resolved_deps = resolved_by_manifest.get(cargo_path.resolve())
        if resolved_deps is None:
            resolved_deps = load_manifest_resolved_dependencies(
                cargo_path.resolve(),
                warn=args.verbose,
            )
            resolved_by_manifest[cargo_path.resolve()] = resolved_deps
        unused_in_crate: list[Dependency] = []

        for dep in parsed.deps:
            total_deps += 1
            rust_name = resolve_rust_name(dep, resolved_deps)

            # Skip optional deps (feature-gated, only used when feature is enabled)
            if dep.optional:
                if args.verbose:
                    print(f"  {rel_path}: [{dep.section}] {dep.name} — skipped (optional/feature-gated)")
                continue

            # Skip crates known to be used implicitly
            if dep.package_name in IMPLICIT_CRATES or dep.name in IMPLICIT_CRATES:
                if args.verbose:
                    print(f"  {rel_path}: [{dep.section}] {dep.name} — skipped (implicit)")
                continue
            if dep.section == "dev-dependencies" and (
                dep.package_name in IMPLICIT_DEV_CRATES or dep.name in IMPLICIT_DEV_CRATES
            ):
                if args.verbose:
                    print(f"  {rel_path}: [{dep.section}] {dep.name} — skipped (implicit dev)")
                continue

            rust_files = collect_rust_sources(crate_dir, dep.section)

            if is_dep_used(dep, rust_name, rust_files, parsed.features_text):
                if args.verbose:
                    print(f"  {rel_path}: [{dep.section}] {dep.name} (-> {rust_name}) — used")
            else:
                unused_in_crate.append(dep)
                print(f"  {rel_path}: [{dep.section}] {dep.name} (-> {rust_name}) — UNUSED")

        if unused_in_crate:
            total_unused += len(unused_in_crate)
            crates_with_unused.append(str(rel_path))

            if args.fix:
                remove_deps_from_toml(parsed, unused_in_crate)
                removed_names = [d.name for d in unused_in_crate]
                print(f"    >> Removed: {', '.join(removed_names)}")

    print(f"\n{'='*60}")
    print(f"Total dependencies scanned: {total_deps}")
    print(f"Unused dependencies found:  {total_unused}")
    print(f"Crates with unused deps:    {len(crates_with_unused)}")
    if crates_with_unused:
        for c in crates_with_unused:
            print(f"  - {c}")

    if not args.fix and total_unused > 0:
        print(f"\nRun with --fix to remove unused dependencies.")
        sys.exit(1)
    elif args.fix and total_unused > 0:
        print(f"\nRemoved {total_unused} unused dependencies.")


if __name__ == "__main__":
    main()
