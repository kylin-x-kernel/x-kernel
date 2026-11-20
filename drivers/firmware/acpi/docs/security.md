# acpi — Security And Reliability

## Trust Boundary

Boot firmware supplies physical addresses. The internal direct-map adapter
assumes these ranges are accessible, immutable, and remain resident for the
lifetime of returned views. It checks address conversion and arithmetic, but
does not validate memory ownership or map new memory.

A checksum cannot prove that a claimed range is mapped: bytes must already be
readable to compute it. Forged lengths or inaccessible addresses remain a
boot-handoff trust limitation. Earlier wording that checksum validation alone
prevented out-of-bounds physical reads was incorrect.

## Decoder Boundary

`vendor/acpi::sdt::borrowed` validates supplied slices without unsafe code or
allocation. It checks the 20-byte RSDP before extended access, both checksums
for revision 2+, SDT minimum/declared length and checksum, root entry framing,
MADT entry bounds, MCFG complete records, and FADT field revision/extent.
Its errors do not select a kernel panic or fallback policy. The typed
`AcpiTables` walk decodes root entries as little-endian bytes, so an XSDT
entry at table offset 36 is never read through an unaligned `u64` reference.

Unknown MADT entries are skipped by declared length. Malformed entries return
one typed table-relative error and stop. ISO flags remain uninterpreted here.
MCFG segment choice and PM1a SystemIO/u16 validation belong to internal queries.

## Unsafe Boundary

Only `raw.rs` forms direct-map byte slices. Internal packed typed header reads
and MCFG typed entry arrays have been removed. Vendor decoders borrow the
provided slices and never extend their extent or lifetime. No interpreter
mapping or OSL lifetime is smuggled into this early path.

## AML Trust Boundary (x86)

The interpreter evaluates trusted boot firmware, not untrusted user input. The
vendored interpreter has no instruction or step budget, so AML that loops or
recurses can consume boot time; this is a trusted-firmware assumption, not a
sandbox.

Boot-time firmware queries cover `\_PIC(1)` once, the root bridge
`_PRT`, the `_CRS` of links referenced by that `_PRT`, and the optional
`\_S3` sleep-type package. Root-bridge probing
also reads `_HID`, `_CID`, `_SEG`, `_BBN`, and `_CRS`. No general driver-facing
AML evaluation API is exported.

The host adapter validates every mapped range and every SystemMemory, SystemIO,
and PCI-config access, rejects operand widths and addresses outside its
contracts, and latches the first failure so later accesses do not reach
hardware. The context polls that latch after every vendor call and evaluation;
host failure wins over a zeroed interpreter result. Unsupported operation-region
spaces are recorded through the host hook and poison the context.

Before a DSDT or SSDT body reaches the interpreter, the context maps the whole
declared table and validates it with vendor `sdt::borrowed::Table::parse` and
`require_signature`: signature, declared extent, and full-table checksum. The
typed vendor enumeration used to find the table does not perform those checks,
so a successful mapping alone is not treated as validation. The expected
signature is `DSDT` for the FADT target and `SSDT` for root entries. A failure
poisons the context and discards the partial namespace.

Protocol field decoding is delegated: `aml::pci_routing::from_object` rejects
malformed `_PRT` package framing, pins outside `0..=3`, and non-DWord addresses;
`aml::resource` frames resource templates with checked arithmetic, requires
the EndTag's trailing checksum byte to be present, selects address-field width
from the descriptor tag,
and decodes IRQ masks, vectors, and the optional `ResourceSource` suffix of an
extended IRQ. Thresholds and acceptance remain internal policy: `prt` caps
`_PRT` at 256 entries, resolves at most 256 distinct links, accepts at most 32
resource descriptors per `_CRS`, requires a complete end tag, and rejects an
IRQ that uses a `ResourceSource` because following that source template is not
implemented. A structurally malformed `_PRT` fails the whole query; a
well-formed key with a bad source or link is stored as a keyed error and never
converted into a valid route. There is no retry, partial table, or fallback
routing table.

## Failure Policy

Optional absence returns None; invalid root/table framing and explicitly
missing required tables retain the existing fatal boot policy. MADT entry
errors remain recoverable by consumers. PM1a absence is nonfatal, but corrupt
RSDP/root data can still panic; the query is not unconditionally panic-free.

## Residual Risks

- Correct checksums do not establish truthful topology.
- Direct-map accessibility is trusted, not proven by table validation.
- The bounded PCI-host DSDT scan is heuristic and advisory, not AML evaluation.
- The vendor's older typed mapping APIs have separate validation limitations;
  this change does not assert they have been hardened by the borrowed path.
- The AML interpreter relies on boot firmware being well-behaved; there is no
  instruction budget or general AML sandbox.
- The x86 context loads every SSDT from the boot table set even though only
  `_PIC`, root `_PRT`, referenced link `_CRS`, and optional `_S3` objects
  are evaluated later.
- No IRQ routing, controller lifecycle, or consumer behavior changes are added.

## Regression Coverage

Host fixtures cover invalid checksums/lengths, partial root/MCFG entries,
MADT termination on errors, short FADT fields and extended GAS address offset,
truncated EndTags, unaligned XSDT entries, and truncated resource descriptors.
No-default-feature tests cover the allocation-independent compilation path.

The x86 unit tests cover table-load failure at each stage, DSDT bad-checksum
and wrong-signature rejection, a revision-2 RSDP/XSDT fixture, `_PIC`
presence, absence, failure, and repeat initialization, host failure and
unsupported-region poisoning, `_PRT` structural and per-key errors, direct and
link routes, descriptor/link bounds, bus-range policy, root-bridge identity and
segment validation, unsupported `ResourceSource` suffixes, and lookup after the
context is dropped.

Vendor AML resource tests own the wire-format regressions: Word/DWord/QWord tag
widths and field offsets, small IRQ masks as bit positions rather than raw
values, extended IRQ multi-vector counting and optional `ResourceSource`
suffixes, unknown-item skipping, and truncated framing including a bare EndTag
byte. Internal tests assert the policy on top of those facts (one IRQ per link,
complete end tag required, source suffixes unsupported, descriptor caps) rather
than re-decoding bytes. The vendor decoder contains no second copy in internal
ACPI.

## Sleep discovery input boundary

Missing/malformed `_S3` packages do not advertise S3. Both sleep types must be
integers in 0..=7. Evaluation errors propagate through existing host/context
poison rules. The FADT FACS accessor obeys declared extent and revision; it does
not dereference FACS or validate a physical mapping. Firmware remains trusted
under boot table policy, and the platform owns exclusive wake-vector lifetime.
