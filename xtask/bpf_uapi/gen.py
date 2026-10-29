#!/usr/bin/env python3
"""Generate the single-source BPF uapi constant module from Linux's bpf.h.

Usage:
    python3 xtask/bpf_uapi/gen.py --linux /path/to/linux [-o core/kbpf/src/uapi.rs]

The generated file is committed, so kernel builds never need a Linux tree.
Re-run this script when the uapi baseline moves, then `git diff` the result:
every changed value is a deliberate ABI decision that must be reviewed.

Only declarations that exist in `include/uapi/linux/bpf.h` are emitted; a
handful of values live elsewhere (`BPF_CGROUP_MAX_PROGS` is a kernel-internal
define in `kernel/bpf/cgroup.c`) and are listed in EXTRA_DEFINES with their
source named.
"""

from __future__ import annotations

import argparse
import re
import sys
from pathlib import Path

HEADER = "include/uapi/linux/bpf.h"

# Values that are not declared in bpf.h; each entry names where it lives.
EXTRA_DEFINES = [
    # (name, expression, source note)
    ("BPF_CGROUP_MAX_PROGS", "64", "kernel/bpf/cgroup.c"),
]

# Enums to emit. Everything else in bpf.h (map flags, test-run flags, ...)
# is emitted too when small; see ENUM_ALLOWLIST.
ENUM_ALLOWLIST = [
    "bpf_cmd",
    "bpf_map_type",
    "bpf_prog_type",
    "bpf_attach_type",
    "bpf_link_type",
    "bpf_stats_type",
    "bpf_task_fd_type",
    "bpf_devcg_acc",
    "bpf_devcg_dev",
    "bpf_map_update_flags",
]

# Anonymous enums in bpf.h that carry ABI values worth emitting, keyed by a
# marker identifier that must appear inside the enum body.
ANON_ENUM_MARKERS = {
    "BPF_DEVCG_ACC_MKNOD": "bpf_devcg_acc",
    "BPF_DEVCG_DEV_BLOCK": "bpf_devcg_dev",
    "BPF_ANY": "bpf_map_update_flags",
}


def strip_comments(text: str) -> str:
    text = re.sub(r"/\*.*?\*/", "", text, flags=re.S)
    return re.sub(r"//[^\n]*", "", text)


def eval_expr(expr: str, known: dict[str, int]) -> int:
    """Evaluate a C integer constant expression against known symbols."""
    expr = expr.strip().rstrip(",").strip()
    # Casts: (1ULL << 0), ((__u32)~0U), (u64)x, (__force u16) ...
    expr = re.sub(r"\(\s*(?:unsigned\s+long\s+long|unsigned\s+long|unsigned|signed|long|int|u64|u32|u16|u8|__u64|__u32|__u16|__u8|__force|__s32|__s16|__s8)\s*\)", "", expr)
    expr = expr.replace("ULL", "").replace("LL", "").replace("UL", "").replace("U", "")
    # `~0` and friends keep working with plain Python ints once unsigned-ness
    # is dropped; callers that need masking apply it via MASKED entries.
    for name in sorted(known, key=len, reverse=True):
        expr = re.sub(rf"\b{re.escape(name)}\b", str(known[name]), expr)
    expr = expr.replace("<<", "<<" ).replace("&&", " and ").replace("||", " or ")
    try:
        return int(eval(expr, {"__builtins__": {}}, {}))  # noqa: S307 - constant folding only
    except Exception as exc:  # pragma: no cover - reported to the caller
        raise ValueError(f"cannot evaluate {expr!r}: {exc}") from exc


def parse_defines(
    text: str,
    known: dict[str, int],
    drop_macros: bool = False,
    skipped: list[str] | None = None,
) -> list[tuple[str, int]]:
    out = []
    for match in re.finditer(r"^[ \t]*#define[ \t]+(BPF_[A-Za-z0-9_]+)[ \t]+(.+)$", text, re.M):
        name, body = match.group(1), match.group(2).strip()
        if drop_macros and "(" in body and "<<" not in body:
            if skipped is not None:
                skipped.append(f"{name} (accessor macro)")
            continue  # BPF_CLASS(code) style accessor macros, not constants
        if body.startswith("BPF_") and body in known:  # plain alias
            known[name] = known[body]
            out.append((name, known[name]))
            continue
        try:
            known[name] = eval_expr(body, known)
        except ValueError:
            if skipped is not None:
                skipped.append(f"{name} = {body}")
            continue
        out.append((name, known[name]))
    return out


def parse_enums(text: str, known: dict[str, int]) -> list[tuple[str, list[tuple[str, int]]]]:
    """Parse named and anonymous enums, preserving declaration order."""
    enums = []
    for match in re.finditer(r"enum\s*([A-Za-z0-9_]*)\s*\{([^}]*)\}", text):
        tag, body = match.group(1), match.group(2)
        if "___BPF_FUNC_MAPPER" in body or "__BPF_FUNC_MAPPER" in body:
            continue  # handled separately
        items: list[tuple[str, int]] = []
        next_value = 0
        explicit = False
        for raw in body.split(","):
            decl = raw.strip()
            if not decl or decl.startswith("#"):
                continue
            if "=" in decl:
                name, expr = decl.split("=", 1)
                name = name.strip()
                try:
                    value = eval_expr(expr, known)
                except ValueError:
                    continue
                explicit = True
            else:
                name = decl
                value = next_value
            known[name] = value
            items.append((name, value))
            next_value = value + 1
        if not items or not explicit and all(v == i for i, (_, v) in enumerate(items)):
            # Anonymous bitmask enums have explicit values; a fully implicit
            # one is still emitted (positional ABI is the point).
            pass
        if not items:
            continue
        if tag:
            enums.append((tag, items))
        else:
            synthetic = None
            for marker, name in ANON_ENUM_MARKERS.items():
                if any(member == marker for member, _ in items):
                    synthetic = name
                    break
            # Every remaining anonymous enum still gets a name: it is part of
            # the uapi surface, and a missing constant is exactly the kind of
            # gap this table exists to prevent. The name is derived from the
            # first member, so it is stable across regenerations.
            if synthetic is None:
                synthetic = "bpf_anon_" + items[0][0].lower()
            enums.append((synthetic, items))
    return enums


def parse_func_ids(text: str) -> list[tuple[str, int]]:
    ids = []
    for match in re.finditer(r"FN\(\s*([A-Za-z0-9_]+)\s*,\s*([0-9]+)", text):
        ids.append((f"BPF_FUNC_{match.group(1)}", int(match.group(2))))
    return ids


def rust_ident(name: str) -> str:
    # Keep the C spelling: these are uapi names and grep-ability matters.
    return name


def rust_type(value: int) -> str:
    """Narrowest type that holds the value.

    Widening only where needed is what lets consumers use the constants
    directly in `match` patterns: a `u64` constant cannot pattern-match a
    `u32` scrutinee, and `x as u32` is not a pattern expression.
    """
    # C enums are `int`/`unsigned int` and the syscall ABI passes them as
    # `u32`; negative enumerators (e.g. `TCX_NEXT = -1`) are signed `int`,
    # and only constants that genuinely need 64 bits are widened.
    if value < 0:
        return "i32"
    if value <= 0xFFFF_FFFF:
        return "u32"
    return "u64"


def emit_str_array(name: str, items: list[str]) -> list[str]:
    """Emit a `&[&str]` const in the shape rustfmt leaves it in.

    The generated file is committed and passes through `make fmt`, so the
    output must already be rustfmt-stable — otherwise `--check` reports a
    stale file that only differs in formatting.
    """
    if not items:
        return [f'pub const {name}: &[&str] = &[];']
    if len(items) == 1:
        return [f'pub const {name}: &[&str] = &["{items[0]}"];']
    lines = [f'pub const {name}: &[&str] = &[']
    lines.extend(f'    "{item}",' for item in items)
    lines.append('];')
    return lines


def emit(enums, defines, func_ids, header_hash: str, skipped: list[str]) -> str:
    lines = [
        "// SPDX-License-Identifier: Apache-2.0",
        "// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>",
        "// See LICENSES for license details.",
        "",
        "//! BPF uapi constants, generated from Linux's `include/uapi/linux/bpf.h`.",
        "//!",
        "//! DO NOT EDIT BY HAND. Regenerate with",
        "//! `python3 xtask/bpf_uapi/gen.py --linux <linux-tree>` and review the diff:",
        f"//! every changed value is an ABI decision. Source header digest: `{header_hash}`.",
        "//!",
        "//! `bpf_uapi_constants_match_linux()` in the tested module pins the values",
        "//! this kernel depends on, independently of the generator.",
        "",
        "#![expect(",
        "    non_upper_case_globals,",
        "    reason = \"generated uapi table keeps the C spellings so a grep for a uapi name\"",
        ")]",
        "",
    ]
    for tag, items in enums:
        lines.append(f"/// C enum `{tag}` (`include/uapi/linux/bpf.h`).")
        lines.append("pub mod " + tag + " {")
        for name, value in items:
            lines.append(f"    pub const {rust_ident(name)}: {rust_type(value)} = {value};")
        lines.append("}")
        lines.append("")
    if defines:
        lines.append("/// `#define` values declared in `include/uapi/linux/bpf.h`.")
        lines.append("pub mod defines {")
        for name, value in defines:
            lines.append(f"    pub const {rust_ident(name)}: {rust_type(value)} = {value};")
        lines.append("}")
        lines.append("")
    else:
        lines.append("/// `#define` values declared in `include/uapi/linux/bpf.h`.")
        lines.append("pub mod defines {}")
        lines.append("")
    lines.append("/// Values this table declares that are NOT from the uapi headers.")
    lines.append("///")
    lines.append("/// Each one is a deliberate exception and needs its source named in")
    lines.append("/// the generator; the conformance test pins this list so a second")
    lines.append("/// exception cannot slip in unnoticed.")
    lines.extend(
        emit_str_array("NON_UAPI_VALUES", [name for name, _e, _s in EXTRA_DEFINES])
    )
    lines.append("")
    lines.append("/// Names of the enum modules in this table.")
    lines.append("pub const ENUM_MODULES: &[&str] = &[")
    for tag, _ in enums:
        lines.append(f'    "{tag}",')
    lines.append("];")
    lines.append("")
    lines.append("/// `#define`s in the uapi headers that are not integer constants.")
    lines.append("///")
    lines.append("/// Listed so a reader can tell \"absent on purpose\" from \"the generator")
    lines.append("/// dropped it\". Function-like accessor macros (`BPF_CLASS(code)`) and")
    lines.append("/// expressions the evaluator cannot fold are the only entries here.")
    lines.extend(
        emit_str_array(
            "NOT_CONSTANTS",
            [entry.replace(chr(34), chr(92) + chr(34)) for entry in skipped],
        )
    )
    lines.append("")
    lines.append("/// `enum bpf_func_id` helper ids (`___BPF_FUNC_MAPPER`).")
    lines.append("pub mod bpf_func_id {")
    for name, value in func_ids:
        lines.append(f"    pub const {rust_ident(name)}: {rust_type(value)} = {value};")
    lines.append("}")
    lines.append("")
    return "\n".join(lines)


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--linux", required=True, type=Path)
    parser.add_argument("--check", action="store_true", help="fail if the generated file differs")
    parser.add_argument("-o", "--output", type=Path,
                        default=Path(__file__).resolve().parents[2] / "core/kbpf/src/uapi.rs")
    args = parser.parse_args()

    header = args.linux / HEADER
    common = args.linux / "include/uapi/linux/bpf_common.h"
    text = strip_comments(header.read_text(encoding="utf-8"))
    # `bpf_common.h` carries the classic ISA bits (classes, sizes, modes,
    # ALU/JMP op nibbles) that `bpf.h` extends; parse it first so `bpf.h`
    # aliases evaluate.
    common_text = strip_comments(common.read_text(encoding="utf-8"))
    digest = __import__("hashlib").sha256(
        header.read_bytes() + common.read_bytes()
    ).hexdigest()[:16]

    known: dict[str, int] = {}
    skipped: list[str] = []
    defines = parse_defines(common_text, known, drop_macros=True, skipped=skipped)
    defines += parse_defines(text, known, skipped=skipped)
    enums = parse_enums(text, known)
    func_ids = parse_func_ids(text)

    # A `#define` the evaluator could not fold may still be defined properly
    # by an enum elsewhere in the same header (`BPF_F_LINK` is a
    # self-referential legacy macro; the enum at the attach-type block gives
    # it its real value). Such names are not gaps, so drop them from the
    # report.
    skipped = [
        entry
        for entry in skipped
        if entry.split(" ")[0] not in known
    ]

    for name, expr, source in EXTRA_DEFINES:
        known[name] = eval_expr(expr, known)
        defines.append((name, known[name]))


    # Every `BPF_*` constant from both headers is emitted: the point of the
    # single source is completeness, and unused constants cost nothing.

    generated = emit(enums, sorted(set(defines)), func_ids, digest, skipped)
    if args.check:
        current = args.output.read_text(encoding="utf-8") if args.output.exists() else ""
        if current != generated:
            print(f"{args.output} is stale; regenerate from the uapi baseline", file=sys.stderr)
            return 1
        print(f"{args.output} is up to date")
        return 0
    args.output.write_text(generated, encoding="utf-8")
    print(f"wrote {args.output} ({len(enums)} enums, {len(defines)} defines, {len(func_ids)} func ids)")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
