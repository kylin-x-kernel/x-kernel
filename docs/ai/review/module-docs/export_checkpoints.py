#!/usr/bin/env python3
"""Export X-Kernel's module-documentation workbook to runtime TSV files."""

from __future__ import annotations

import argparse
import csv
import io
import posixpath
import re
import sys
import zipfile
from pathlib import Path
from xml.etree import ElementTree as ET


SCRIPT_PATH = Path(__file__).resolve()
MODULE_DOCS_DIR = SCRIPT_PATH.parent
DEFAULT_WORKBOOK = MODULE_DOCS_DIR / "check.xlsx"
DEFAULT_OUTPUT = MODULE_DOCS_DIR
HEADERS = ["规则 ID", "等级", "检查点", "合格标准", "问题判定", "错误示例"]
SHEETS = {
    "design": ("DD", "design-doc-checks.tsv"),
    "security": ("SD", "security-doc-checks.tsv"),
    "rustdoc": ("RD", "rustdoc-checks.tsv"),
}
MAIN_NS = "http://schemas.openxmlformats.org/spreadsheetml/2006/main"
REL_NS = "http://schemas.openxmlformats.org/officeDocument/2006/relationships"
PKG_REL_NS = "http://schemas.openxmlformats.org/package/2006/relationships"


class ExportError(RuntimeError):
    pass


def qname(namespace: str, name: str) -> str:
    return f"{{{namespace}}}{name}"


def column_index(reference: str) -> int:
    match = re.match(r"([A-Z]+)", reference)
    if not match:
        raise ExportError(f"invalid cell reference: {reference}")
    result = 0
    for char in match.group(1):
        result = result * 26 + ord(char) - ord("A") + 1
    return result - 1


def shared_strings(archive: zipfile.ZipFile) -> list[str]:
    try:
        root = ET.fromstring(archive.read("xl/sharedStrings.xml"))
    except KeyError:
        return []
    return ["".join(node.text or "" for node in item.iter(qname(MAIN_NS, "t"))) for item in root]


def cell_value(cell: ET.Element, strings: list[str]) -> str:
    cell_type = cell.get("t")
    if cell_type == "inlineStr":
        return "".join(node.text or "" for node in cell.iter(qname(MAIN_NS, "t")))
    value = cell.find(qname(MAIN_NS, "v"))
    raw = value.text if value is not None and value.text is not None else ""
    if cell_type == "s" and raw:
        return strings[int(raw)]
    if cell_type == "b":
        return "TRUE" if raw == "1" else "FALSE"
    return raw


def workbook_rows(path: Path) -> dict[str, list[list[str]]]:
    try:
        archive = zipfile.ZipFile(path)
    except (OSError, zipfile.BadZipFile) as exc:
        raise ExportError(f"cannot open workbook {path}: {exc}") from exc

    with archive:
        strings = shared_strings(archive)
        workbook = ET.fromstring(archive.read("xl/workbook.xml"))
        relationships = ET.fromstring(archive.read("xl/_rels/workbook.xml.rels"))
        targets = {
            relation.get("Id"): relation.get("Target")
            for relation in relationships.findall(qname(PKG_REL_NS, "Relationship"))
        }
        result: dict[str, list[list[str]]] = {}
        for sheet in workbook.find(qname(MAIN_NS, "sheets")) or []:
            name = sheet.get("name", "")
            relation_id = sheet.get(qname(REL_NS, "id"), "")
            target = targets.get(relation_id)
            if not target:
                raise ExportError(f"worksheet target is missing: {name}")
            normalized_target = target.lstrip("/")
            worksheet_path = (
                posixpath.normpath(normalized_target)
                if normalized_target.startswith("xl/")
                else posixpath.normpath(posixpath.join("xl", normalized_target))
            )
            worksheet = ET.fromstring(archive.read(worksheet_path))
            rows: list[list[str]] = []
            sheet_data = worksheet.find(qname(MAIN_NS, "sheetData"))
            if sheet_data is None:
                result[name] = rows
                continue
            for row in sheet_data.findall(qname(MAIN_NS, "row")):
                values = [""] * len(HEADERS)
                for cell in row.findall(qname(MAIN_NS, "c")):
                    index = column_index(cell.get("r", ""))
                    if index < len(HEADERS):
                        values[index] = cell_value(cell, strings)
                if any(values):
                    rows.append(values)
            result[name] = rows
        return result


def render_tsv(rows: list[list[str]], sheet: str, prefix: str) -> str:
    if not rows or rows[0] != HEADERS:
        raise ExportError(f"{sheet}: expected header {HEADERS}, got {rows[0] if rows else []}")
    seen: set[str] = set()
    for number, row in enumerate(rows[1:], start=2):
        if len(row) != len(HEADERS) or any(not value.strip() for value in row):
            raise ExportError(f"{sheet}!{number}: all six cells must be non-empty")
        rule_id, level = row[0], row[1]
        if not re.fullmatch(rf"{prefix}-\d{{3}}", rule_id):
            raise ExportError(f"{sheet}!{number}: invalid rule id {rule_id}")
        if rule_id in seen:
            raise ExportError(f"{sheet}!{number}: duplicate rule id {rule_id}")
        if level not in {"MUST", "SHOULD", "MAY"}:
            raise ExportError(f"{sheet}!{number}: invalid level {level}")
        if any("\t" in value or "\n" in value or "\r" in value for value in row):
            raise ExportError(f"{sheet}!{number}: cells cannot contain tabs or newlines")
        seen.add(rule_id)

    output = io.StringIO(newline="")
    writer = csv.writer(output, delimiter="\t", lineterminator="\n")
    writer.writerows(rows)
    return output.getvalue()


def export(workbook: Path, output: Path, check: bool) -> None:
    sheets = workbook_rows(workbook)
    if set(sheets) != set(SHEETS):
        raise ExportError(f"expected worksheets {sorted(SHEETS)}, got {sorted(sheets)}")
    output.mkdir(parents=True, exist_ok=True)
    stale: list[str] = []
    for name, (prefix, filename) in SHEETS.items():
        content = render_tsv(sheets[name], name, prefix)
        target = output / filename
        if check:
            try:
                actual = target.read_text(encoding="utf-8")
            except OSError:
                actual = ""
            if actual != content:
                stale.append(str(target))
        else:
            target.write_text(content, encoding="utf-8")
    if stale:
        raise ExportError("runtime checkpoint exports are stale: " + ", ".join(stale))
    action = "checked" if check else "exported"
    print(f"{action} {len(SHEETS)} checkpoint sheets from {workbook}")


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--workbook", type=Path, default=DEFAULT_WORKBOOK)
    parser.add_argument("--output", type=Path, default=DEFAULT_OUTPUT)
    parser.add_argument("--check", action="store_true")
    args = parser.parse_args()
    try:
        export(args.workbook.resolve(), args.output.resolve(), args.check)
    except ExportError as exc:
        print(f"error: {exc}", file=sys.stderr)
        return 2
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
