# acpi — Design

## Ownership

This crate integrates ACPI into kernel boot. It is not a second ACPI protocol
library. Generic protocol decoding lives in `vendor/acpi`, imported as
`acpi-rs`: static table formats in `sdt::borrowed`, and evaluated-object formats
in `aml::pci_routing` and `aml::resource`. This crate applies firmware-query
policy, support scope, boot access, and failure handling to those decoded facts.
It must not grow a parallel parser when the vendor interface is inconvenient.

| Component | Owns | Does not own |
| --- | --- | --- |
| vendor `sdt::borrowed` | Strict RSDP/SDT validation, root traversal, bounded MADT/MCFG/FADT and address-descriptor decoding | Physical mappings, boot globals, panic policy, IRQ routing |
| vendor `aml::pci_routing` | `_PRT` package structure, address/pin fields, source classification, retained source-index facts | Root-bridge selection, source-index support scope, route defaults, keyed error policy |
| vendor `aml::resource` | Resource-template framing, descriptor types and widths, IRQ mask/vector and address-space field decoding | Descriptor-count caps, which descriptor is usable, trigger naming, boot policy |
| internal `desc` | One-shot boot RSDP address | Table formats |
| internal `raw` / `table` | Direct-map access, compatibility header snapshot, required/optional lookup and fatal corruption policy | Checksums, root entry framing, packed firmware layouts |
| internal `madt` | Boot lookup and public re-exports of vendor facts | MADT decoding or IRQ-controller policy |
| internal `mcfg` | First-segment-zero selection | MCFG record decoding |
| internal `fadt` | SystemIO/nonzero/u16-port selection for the existing power path | FADT offsets or GAS decoding |
| internal `pci_window` | Transitional bounded search for a PCI host window | Resource descriptor field decoding or AML interpretation |
| internal `host` | AML host failure vocabulary | OSL state, interpreter ownership |
| internal `context` (x86) | One AML evaluation owner: table loading, `_PIC`, serialized namespace access, host-failure poisoning | AML object/resource field decoding, PCI routing policy, consumer wiring |
| internal `prt` (x86) | Root-bridge selection, `_SEG`/`_BBN`/bus-range support scope, link resolution policy, descriptor/link caps, keyed error policy, owned lookup table | `_PRT`/`_CRS` wire-format decoding, PCI configuration, GSI/IO-APIC mapping, consumer routing |
| xkernel provider | OSL operations through existing OS capabilities | Table formats or PCI-host selection |
| IRQ consumers | Controller selection, GSI/ISO interpretation, domain setup | ACPI table wire formats |

"ACPI-related" is not sufficient to assign ownership. Code named `context`,
`adapter`, or `service` is judged by its actual responsibilities. When the
vendor interface lacks a required capability, the correct move is to add that
capability in the vendor layer; copying the implementation upward to bypass the
boundary is not allowed.

## Ownership Checklist

The ownership of each ACPI capability is recorded here:

| New capability | Owning component | State/lifecycle owned | Existing interfaces reused | Explicitly not responsible for |
| --- | --- | --- | --- | --- |
| AML table/namespace loading and `_PIC` | internal `context` | `AcpiTables`, `Interpreter`, IRQ model, poison latch | vendor `AcpiTables`, `Interpreter`, `FixedRegisters`, `AmlHost` | `_PRT`/`_CRS` formats, route selection |
| `_PRT` package decoding | vendor `aml::pci_routing` | Temporary decoded entry vector | AML `Object`/`Package` API | Root discovery, `SourceIndex` policy, route defaults |
| Link `_CRS` framing and descriptor decoding | vendor `aml::resource` | Temporary decoded descriptors | `resource_descriptors`, `ResourceDescriptor::decode` | Descriptor caps, one-IRQ choice, trigger naming |
| Root-bridge selection, support scope, error policy, owned table | internal `prt` | `PrtTable`, per-query link cache | context evaluation helpers and vendor decoders | AML execution internals, PCI config, GSI mapping |
| AML host access and failure latching | xkernel provider `acpi_osl` | Failure slot, blocked-access flag | existing kernel memory/port/PCI/time capabilities | Firmware interpretation, route policy |

## Vendor Reuse Audit

`prt.rs` contains only policy and orchestration; `_PRT` and `_CRS` decoding
lives in the vendor layer.

- `_PRT`: `PciRoutingTable::from_object` decodes the package, address, pin,
  and source into `PciRouteSource::Gsi` or
  `PciRouteSource::Link { name, index }`. `prt` selects the single supported
  root bridge, requires source index zero for links, applies the direct-GSI
  `LevelLow` default, stores per-key failures, and builds the owned table.
- `_CRS`: `resource_descriptors` frames small and large items with checked
  arithmetic and requires the EndTag's trailing checksum byte to be present;
  `ResourceDescriptor::decode`
  selects address-field width from the descriptor tag (Word 0x08, DWord 0x07,
  QWord 0x0A), decodes small IRQ masks as bitmask-to-IRQ-number (not the raw
  mask value), decodes all extended IRQ vectors, and retains an extended IRQ's
  optional `ResourceSource` index/string as a decoded fact. `prt` applies the
  32-item cap, requires a complete end tag, requires exactly one IRQ number
  for a link, maps descriptor facts to `PrtTrigger`, and rejects a
  `ResourceSource` suffix as unsupported policy rather than malformed format.
- AML table loading validates each DSDT/SSDT through vendor
  `sdt::borrowed::Table::parse` plus `require_signature` before handing the
  body to the interpreter. The vendor typed `AcpiTables` path does not verify
  these, so the internal layer does not treat a successful mapping as
  validation; it owns only the length cap and the expected signature choice.
- Root-entry traversal decodes RSDT/XSDT entries bytewise. XSDT entries start
  at table offset 36, so forming a `u64` reference there would be unaligned.
- `pci_window` already uses the vendor borrowed address descriptor; the
  transitional scan reads decoded facts, not packed fields.
- The vendor `PciRoutingTable::route` and `from_prt_path` helpers were removed.
  They evaluated AML and hard-coded the direct-GSI level/active-low/shared
  default plus source-index-zero support, so keeping them would have left a
  second route path that mixed interpreter and platform policy. Internal `prt`
  consumes only `from_object` facts and owns those decisions.
- No parallel decoder remains in internal ACPI (`decode_prt`, `decode_source`,
  `walk_crs`, `irq_from_mask`, and `decode_extended_irq` were deleted).

Regression facts follow the same split. Vendor tests cover framing (including
a truncated EndTag), descriptor tags and widths, IRQ mask semantics, extended
vectors and their optional `ResourceSource` suffix, unknown-item skipping,
truncation, and unaligned XSDT entry decoding. Internal tests cover root-bridge
selection, `_SEG`/`_BBN`/bus scope, one-IRQ and end-tag policy, unsupported
source suffixes, error keying, caps, AML signature/checksum rejection, and
host-failure propagation. The real XSDT fixture places its first 8-byte entry
at an offset that is not 8-byte aligned. The removed internal decoder had
swapped Word/DWord width tags and computed address fields at the wrong
granularity/minimum offsets; the vendor width/offset tests now pin the correct
layout.

## Acceptance Gates

- Dependency direction is checkable: vendor `acpi` must not import host kernel
  crates, and internal ACPI must not depend on concrete provider, KIRQ, or
  controller implementations. The firmware crate depends on vendor `acpi`,
  low-level address helpers, and the `AmlHost` contract only.
- Review of internal ACPI changes must inspect new byte offsets, descriptor-tag
  matches, and package field splitting. Ordinary byte operations are not
  forbidden, but protocol field interpretation in internal ACPI is a boundary
  violation. CI is expected to emit review hints for those patterns rather
  than mechanically reject every byte operation. (Automated hinting is not yet
  wired; it remains a follow-up.)
- Review checks correctness, implementation location, interface contract, and
  duplicate capability. Unfinished production wiring, missing tests, and
  deferred compatibility removal must be stated explicitly in the design.
- Ambiguous ownership blocks implementation. Passing behavior does not
  justify an out-of-boundary implementation.

The existing public imports remain valid. MADT facts and MCFG allocation facts
are re-exported; `AcpiTableHeader` stays a compatibility snapshot because its
signature and creator fields differ from the vendor type.

## Static Access Flow

```text
explicit early RSDP OR desc::rsdp_addr()
  -> internal table::find_optional
  -> vendor borrowed::find_table
       -> callback for kernel direct-map byte access
       -> RSDP basic/extended checksum, XSDT/RSDT traversal, SDT checks
  -> vendor bounded table-specific decoding
  -> internal compatibility selection
  -> platform / IRQ / PCI / power consumers
```

The static path has no allocator, scheduler, OSL, or interpreter requirement.
The x86 CPU handoff can still read MADT before global ACPI initialization.
Post-init static queries use the same path, not an interpreter-owned duplicate.

The borrowed vendor API is distinct from its typed `AcpiTables<Handler>` path:
it accepts checked slices, including unaligned and short revision-dependent
tables, and returns typed errors. It does not require forming a reference to a
full modern FADT or implementing every AML host operation during early boot.
Existing vendor typed APIs are not globally replaced in this change. In
particular this does not claim to fix their separate validation limitations.

## Interpreter Access Flow (x86)

```text
AcpiContext::new / from_rsdp(host)
  -> vendor AcpiTables::from_rsdp
  -> FADT + FixedRegisters
  -> vendor Interpreter::new
  -> map and load DSDT, then every enumerated SSDT
  -> init_irq_model(IoApic)
       -> evaluate \_PIC(1) once (absent is tolerated)
  -> prt::load(&mut context, bus = 0)
       -> locate one PNP0A03/PNP0A08 device
       -> validate _SEG, _CRS bus range, and _BBN
       -> evaluate root _PRT once
       -> resolve each link and decode its _CRS
       -> return an owned, bounded PrtTable
```

This path is x86-only and allocation-based. It is separate from the early
static path; it exists only after the boot allocator. The context owns the only
interpreter and exposes no namespace or object handle. Evaluation is serialized
by `&mut self`, and decode closures must copy what they need before returning.

`prt::load` never evaluates `_PIC`; the owner must call `init_irq_model` first.
The returned `PrtTable` contains fixed-size copied entries and no vendor object
or firmware link name, so lookup remains valid independently of the context.
The firmware layer stops at the query; PCI INTx resolution and consumer routing
belong to `kdriver` (`bus::intx`).

### Host failure and poisoning

The vendor interpreter returns raw handler values and cannot stop itself after
a failed access. The OSL latches the first failure and blocks later hardware
access. The context polls the host after table construction, namespace
loading, and every evaluation; a host failure takes precedence over any AML or
type error and poisons the context. An unsupported operation-region space that
the interpreter rejects before calling the handler is reported through
`AmlHost::note_unsupported_region` and poisons the same way. A `_PIC` failure or
an AML load failure poisons with the firmware policy. Once poisoned, no later
operation returns partial data.

The vendored `AcpiTables` header/table enumeration silently skips mapping
failures with `.ok()?`. This crate does not treat those skips as "table absent":
it polls the host after enumeration, cross-checks the SSDT count against the
number it attempted to load, and returns `IncompleteTableSet` when a clean host
still produced an incomplete set.

## Lifetime And Trust

The kernel owns the boot firmware's accessible, immutable direct-map storage.
Vendor views only borrow that storage. The access callback checks integer
conversion, pointer arithmetic and slice-size bounds; it does not establish
that a firmware-supplied physical range is RAM or mapped. That remains a boot
handoff trust assumption, not something a checksum can prove.

RSDP reads start with 20 bytes, then read the extended header only for revision
2 or later. SDT reads first obtain the fixed header length. Complete-table
checksums precede body decoding. No internal packed structs or typed entry
slices remain. The borrowed decoders contain no unsafe code.

## Errors And Compatibility

- Optional table absence returns `None`; explicitly required tables panic
  when absent. Invalid RSDP/root/SDT data still triggers the existing fatal
  boot policy in the integration layer. Vendor decoders never choose panic
  as a firmware-error policy.
- MCFG and PM1a compatibility probes also retain `None` for an RSDP with
  neither root address. This does not suppress malformed existing roots.
- MADT entry framing returns one table-relative error and terminates;
  unknown framed entries are skipped, and ISO flags remain raw.
- MCFG's first segment-zero choice stays local. Partial records now reject
  the query instead of being silently truncated. ECAM arithmetic also checks
  the final byte of the covered range.
- Root tables with partial entries now return an error instead of dropping
  trailing bytes. RSDT/XSDT entries are decoded as little-endian bytes; an XSDT
  entry at table offset 36 is read without an unaligned `u64` reference.
- The AML context validates every DSDT/SSDT with vendor
  `sdt::borrowed::Table::parse` and `require_signature` before interpreter
  loading. Corrupted checksums or a DSDT whose signature is not `DSDT` fail
  with `InvalidAmlTable`; a successful mapping is not treated as validation.
  The deliberate RSDP-checksum tolerance in the vendor constructor is
  unchanged.
- FADT GAS addresses are decoded at GAS offset 4, correcting the old PM1a
  offset-8 bug. Extended fields require both sufficient length and revision.
  SystemIO/port-range selection remains local; the legacy fallback remains.
- A typed read-only SDT mapping may name a range shorter than `size_of::<T>()`
  (a revision-bounded table such as an ACPI 2.0 FADT). The OSL treats the
  declared length as the vendor's requested view but maps the whole `T`, so
  `ReadOnlyPhysicalMapping`'s validity contract holds; the vendor interprets
  only the fields the table revision declares. Writable mappings keep the
  strict `size >= size_of::<T>()` rule and are not extended.
- PM1a absence is nonfatal, but corrupt root tables have always been able to
  panic. Previous descriptions calling this query unconditionally panic-free
  were incorrect. This change does not introduce a new shutdown failure policy.

## Compatibility Debt

The DSDT `_CRS` byte scan is not AML evaluation. It retains the bounded
PNP0A08/PNP0A03 and `_CRS` search and selects the first usable non-prefetchable
memory window using vendor address-descriptor facts. The result is advisory.
Remove it only after a real PCI root-bridge address-resource query exists and
`pci_bar_range` consumers migrate. Link IRQ `_CRS` support does not replace it.

## Interpreter Ownership

The interpreter context is generic over `H: acpi_rs::Handler + AmlHost`. The
concrete owner is:

- Type: `kdriver::bus::acpi_owner` (x86 only), holding
  `acpi::context::AcpiContext<driver_os_api_xkernel::acpi_osl::AcpiOs>` and the
  loaded `acpi::prt::PrtTable` in one `OwnerState`.
- Instance: one `static OWNER: LazyInit<OwnerState>` in that module. The state
  is either `Ready { ctx, table }` or `Failed(InitFailure)`, a compact
  initialized-context / `_PIC` / `_PRT` classification. The full typed
  `ContextError`/`PrtError` is logged once during initialization; the snapshot
  keeps the failure publishable from a `Sync` static and reachable from the
  PCI INTx resolver's route queries.
- Construction: `kdriver::bus::pci_backend::PciBackend::enumerate` (the
  `BusBackend` implementation). After `PciBus::new` succeeds and `origin` is
  computed, inside the `DiscoveryOrigin::Acpi` branch and before Pass 1,
  construct it with `AcpiOs::for_kernel(Some(*bus.parts_mut().1))`, execute
  `_PIC(1)`, and load the root `_PRT`.

`PciBackend::enumerate` runs during `kdriver::init_drivers()` in late init,
after the allocator and after `khal::firmware::init()` has called `acpi::init()`.
That satisfies the lifetime rule. The owner is not created by `prt` and the
firmware crate does not depend on the provider crate.

`LazyInit::call_once` runs the constructor outside any lock, so `_PIC` and
`_PRT` evaluation is not performed under a spinlock. Once published, route
queries use `LazyInit::get` and `PrtTable::lookup` only; they do not lock or
evaluate the context. A failed initialization is retained for the boot, and a
new enumeration does not retry it. The context is retained with the table so
the host/OSL lifetime remains valid; it is not a per-lookup lock.

The PCI INTx route producer is not a `khal` seam. `pci` owns the Interrupt Pin
read and `LegacyInterruptRoute`; `kdriver::bus::acpi_owner` publishes the x86
`Gsi` lookup from the retained table; the non-x86 DT `Hwirq` producer stays in
`pci`. Reading the pin, mapping the route, and recording the result belong to
the PCI integration (`kdriver::bus::intx`, `bus::pci_backend`): the outcome is
a driver-neutral `IntxRouteStatus` stored on the device, not a global BDF
table, and probe-error policy belongs to the VirtIO integration.

## Validation

Vendor tests cover basic/extended checksum rejection, RSDT/XSDT lookup and
unaligned XSDT entry decoding, absence, malformed lengths, partial root and
MCFG entries, MADT framing, short/revision-dependent FADT access, the PM1a
GAS-offset regression, truncated EndTags, extended IRQ vectors and their
optional `ResourceSource` suffix, and resource truncation. The x86 context
tests load a real-layout XSDT fixture and reject DSDT tables with a bad
checksum or a non-DSDT signature. Run both `make check_acpi` and the vendor
host tests with `--no-default-features`; the latter compiles the decoder
without alloc/AML. Kernel builds validate existing callers and compatibility
re-exports.

## System sleep discovery

The bounded borrowed FADT view selects a complete revision-appropriate extended
FACS pointer or its nonzero legacy fallback. Platform code separately validates
FACS signature/extent before writing wake fields. AcpiContext copies the first
two bounded integer sleep types from `_S3` while the boot AML owner is still
exclusive. The integration owner publishes only this copied fact, so the PM
critical interval does not allocate or evaluate AML. This query does not execute
platform-specific `_PTS`/`_WAK` methods or advertise general physical-platform PM.
