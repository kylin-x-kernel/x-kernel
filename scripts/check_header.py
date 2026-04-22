#!/usr/bin/env python3
"""
Scan all .rs files in the workspace for the expected license header.
Optionally add missing headers with --fix.

Usage:
    # Check only (default) — report missing headers, exit 1 if any found
    python3 scripts/check_header.py

    # Actually add missing headers
    python3 scripts/check_header.py --fix
"""

import argparse
import os
import sys


EXPECTED_HEADER = """// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details."""


def find_project_root():
    """Find the workspace root by looking for the root Cargo.toml with [workspace]."""
    script_dir = os.path.dirname(os.path.abspath(__file__))
    parent = script_dir
    while True:
        cargo_toml = os.path.join(parent, "Cargo.toml")
        if os.path.exists(cargo_toml):
            with open(cargo_toml, 'r') as f:
                if "[workspace]" in f.read():
                    return parent
        next_parent = os.path.dirname(parent)
        if next_parent == parent:
            break
        parent = next_parent
    return os.getcwd()


def check_rs_file_headers(root_dir):
    """
    Check all .rs files under root_dir for the expected license header.
    Returns a list of file paths (relative to root_dir) missing the header.
    """
    missing = []

    for root, dirs, files in os.walk(root_dir):
        if 'target' in dirs:
            dirs.remove('target')
        if '.git' in dirs:
            dirs.remove('.git')

        for fname in files:
            if not fname.endswith(".rs"):
                continue

            file_path = os.path.join(root, fname)
            rel_path = os.path.relpath(file_path, root_dir)
            try:
                with open(file_path, 'r', encoding='utf-8') as f:
                    head = "".join(f.readline() for _ in range(10))
                    if EXPECTED_HEADER not in head:
                        missing.append(rel_path)
            except Exception as e:
                print(f"  {rel_path} — failed to read: {e}")

    return missing


def add_missing_headers(root_dir, missing_list):
    """
    Add the license header to files that are missing it.
    """
    for rel_path in missing_list:
        file_path = os.path.join(root_dir, rel_path)
        try:
            with open(file_path, 'r', encoding='utf-8') as f:
                content = f.read()

            with open(file_path, 'w', encoding='utf-8') as f:
                f.write(EXPECTED_HEADER + "\n\n" + content)
            print(f"    >> Added: {rel_path}")
        except Exception as e:
            print(f"    >> Failed: {rel_path}: {e}")


def main():
    parser = argparse.ArgumentParser(
        description="Find and optionally add missing license headers in .rs files",
    )
    parser.add_argument("--fix", action="store_true",
                        help="Add missing headers (default: check only)")
    args = parser.parse_args()

    root = find_project_root()
    print(f"Project root: {root}\n")

    missing = check_rs_file_headers(root)

    print(f"{'=' * 60}")
    print(f"Total files missing header: {len(missing)}")
    if missing:
        for path in missing:
            print(f"  {path}")

    if missing:
        if args.fix:
            print()
            add_missing_headers(root, missing)
            print(f"\nAdded headers to {len(missing)} file(s).")
        else:
            print(f"\nRun with --fix to add missing headers.")
            sys.exit(1)
    else:
        print("All .rs files have the expected license header.")


if __name__ == "__main__":
    main()
