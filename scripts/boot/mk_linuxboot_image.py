#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0

import argparse
import math
import re
import struct
import subprocess
from pathlib import Path

SETUP_SECTORS_OFFSET = 0x1F1
SYSSIZE_OFFSET = 0x1F4
PAYLOAD_OFFSET_OFFSET = 0x248
PAYLOAD_LENGTH_OFFSET = 0x24C
INIT_SIZE_OFFSET = 0x260
SECTOR_SIZE = 512


def symbol_value(elf_path: Path, name: str) -> int:
    output = subprocess.check_output(["nm", "-n", str(elf_path)], text=True)
    pattern = re.compile(rf"^([0-9a-fA-F]+)\s+\w\s+{re.escape(name)}$")
    for line in output.splitlines():
        match = pattern.match(line.strip())
        if match:
            return int(match.group(1), 16)
    raise RuntimeError(f"missing symbol {name} in {elf_path}")


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--setup", required=True, type=Path)
    parser.add_argument("--stub-elf", required=True, type=Path)
    parser.add_argument("--stub-bin", required=True, type=Path)
    parser.add_argument("--kernel", required=True, type=Path)
    parser.add_argument("--output", required=True, type=Path)
    args = parser.parse_args()

    setup = bytearray(args.setup.read_bytes())
    setup_sectors = max(5, math.ceil(len(setup) / SECTOR_SIZE))
    setup_padded = setup + bytes(setup_sectors * SECTOR_SIZE - len(setup))

    image_start = symbol_value(args.stub_elf, "__image_start")
    image_end = symbol_value(args.stub_elf, "__image_end")
    stub_image_size = image_end - image_start
    stub_file = args.stub_bin.read_bytes()
    if len(stub_file) > stub_image_size:
        raise RuntimeError("flat bootstub exceeds linked image size")
    stub_padded = stub_file + bytes(stub_image_size - len(stub_file))

    kernel = args.kernel.read_bytes()
    protected_mode = stub_padded + kernel

    struct.pack_into("<B", setup_padded, SETUP_SECTORS_OFFSET, setup_sectors - 1)
    struct.pack_into("<I", setup_padded, SYSSIZE_OFFSET, math.ceil(len(protected_mode) / 16))
    struct.pack_into("<I", setup_padded, PAYLOAD_OFFSET_OFFSET, stub_image_size)
    struct.pack_into("<I", setup_padded, PAYLOAD_LENGTH_OFFSET, len(kernel))
    struct.pack_into("<I", setup_padded, INIT_SIZE_OFFSET, len(protected_mode))

    args.output.write_bytes(setup_padded + protected_mode)


if __name__ == "__main__":
    main()
