# kernel-image-metadata — Security and Reliability Analysis

## Trust Model

Field values constructed by XKMake are trusted build inputs, but the sections, note headers,
program headers, and file ranges of the ELF being modified must all be validated. The kernel side treats the image descriptor as potentially corrupted
data and does not print it before the completeness boundary, CRC32, and UTF-8 checks have passed.

## External Boundaries / Attack Surface

The host boundary is the ELF file produced by Cargo; the kernel boundary is the boot
image memory described by linker symbols. The crate does not touch user memory, MMIO, DMA, devices, the file system, or network input.

## unsafe Code Inventory

`linker_slice` converts linker symbol addresses into a static slice. Its safety depends on the final
linker script placing the fixed reserve bounded by the start/end symbols within the same initialized,
read-only, loaded `PT_LOAD` range; the function first validates the address order and non-zero length, and then calls
`from_raw_parts`. XKMake validates the complete reserve during finalize and bundle reuse, rather than
validating only the shortened visible note section.

## Memory Safety Invariants

- The start of the linker descriptor is not greater than its end, and the range lies within the mapped kernel image.
- All encoded fields use explicit little-endian byte order and do not depend on the Rust ABI or struct layout.
- The payload end position is validated using checked arithmetic and slice `get`.
- A `&str` is returned only after UTF-8 validation succeeds.

## Thread Safety

The kernel image is fully written before boot and is read-only at runtime. The crate has no internal synchronization and no global mutable state.

## Threat Analysis

| ID | Threat Description | Impact Level | Trigger Condition | Mitigation |
|------|----------|----------|----------|----------|
| T-01 | Forged ELF fields cause an out-of-bounds write on the host | High | Input ELF section/header is corrupted | XKMake validates section file ranges, sizes, and load segment containment |
| T-02 | A wrong linker range causes an out-of-bounds read in the kernel | High | The linker script is inconsistent with the crate ABI | Fixed symbol names, address order checks, and section validation at build time |
| T-03 | A corrupted payload is printed as a string | Medium | Image bytes are corrupted | The string is returned only after both CRC32 and UTF-8 checks pass |
| T-04 | CRC32 is mistaken for an authenticity guarantee | Low | The image has been maliciously tampered with | CRC32 only detects accidental corruption; authenticity is the responsibility of secure boot or image signing |

## Failure Mode and Effects Analysis (FMEA)

| ID | Failure Mode | Failure Cause | Local Effect | System Effect | Severity | Mitigation |
|------|----------|----------|----------|----------|--------|----------|
| F-01 | Payload exceeds the reserved space | Field growth | Bundle build fails | No new bundle is published | 3 | Encoding returns `PayloadTooLarge` |
| F-02 | Note is not in a load segment | Linker rule regression | ELF post-processing fails | No boot image is generated | 2 | XKMake explicitly validates program header containment |
| F-03 | Boot-time validation fails | ELF corruption or protocol mismatch | Metadata is unavailable | The kernel continues booting and prints an error | 4 | The entry treats metadata as diagnostic information |

## Fault Management

All format APIs return through `Result`. A host failure aborts promotion of the temporary bundle; a kernel
read failure only affects diagnostic output and does not prevent subsequent initialization.

## Privacy Analysis

The payload may contain the build username and hostname. Official releases can override or clear this field through the build environment;
this information must not be treated as input to runtime security decisions.

## Known Limitations

- CRC32 provides no tamper resistance.
- Format version 1 supports only UTF-8 text payloads.
- The build-info descriptor is fixed at 1024 bytes.

## Audit Checklist

- When changing a note, header, or symbol name, are the producer, consumer, and linker updated together?
- Does every new ELF write path validate that the section lies within `PT_LOAD`?
- Is the Build ID written last, after all loadable content is complete?
- Does the kernel always treat read failures as recoverable diagnostic errors?
