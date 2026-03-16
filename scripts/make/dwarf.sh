#!/usr/bin/env bash

ELF=$1
OBJCOPY=$2

if [ -z "$ELF" ] || [ -z "$OBJCOPY" ]; then
    echo "Usage: $0 <elf-file> <objcopy-command>"
    exit 1
fi

if [ ! -f "$ELF" ]; then
    echo "Error: ELF file '$ELF' does not exist."
    exit 1
fi

SECTIONS=(
    debug_abbrev
    debug_addr
    debug_aranges
    debug_info
    debug_line
    debug_line_str
    debug_ranges
    debug_rnglists
    debug_str
    debug_str_offsets
)

# Step 1: Dump all .debug_* sections to temporary files (in parallel)
for section in "${SECTIONS[@]}"; do
    $OBJCOPY "$ELF" --dump-section ".$section=$section.bin" 2> /dev/null || touch "$section.bin" &
done
wait

# Step 2: Strip debug info from the ELF.
# This removes all .debug_* sections and the SHT_SYMTAB_SHNDX table
# that references them, producing a clean ELF.
$OBJCOPY "$ELF" --strip-debug

# Step 3: Re-add the debug data as non-dot-prefixed sections (e.g. "debug_info")
# using --add-section instead of --update-section + --rename-section.
# This avoids the llvm-objcopy bug where SHN_XINDEX entries become
# inconsistent after combined update+rename operations.
cmd=($OBJCOPY "$ELF")
for section in "${SECTIONS[@]}"; do
    if [ -s "$section.bin" ]; then
        cmd+=(--update-section "$section=$section.bin")
    fi
done
"${cmd[@]}"

# Step 4: Clean up temporary files
for section in "${SECTIONS[@]}"; do
    rm -f "$section.bin"
done
