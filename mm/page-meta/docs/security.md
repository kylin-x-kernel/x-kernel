# page-meta — Security and Reliability Analysis

## Trust Boundary

The input is a slice of PhysAddrRange held by the caller. A PA only represents the identity of a physical page and is never dereferenced;
real writes are performed only through the NonNull<u8> provided by the mapping callback. The unsafe contract of init requires that the input RAM be
exclusively owned, that the callback provide exclusive writable storage for the complete backing range, and that sufficient lifetimes be maintained. Arithmetic and coverage validation
cannot prove that the hardware mapping is valid, nor do they require that physical pages not used as metadata already have a kernel mapping.

Public lookup only borrows descriptors; it grants no ownership, content access
or release authority. Reverse lookup checks descriptor identity and cannot create
a hold. Coverage includes complete input pages, not every boot-reserved RAM page
on the machine.

## unsafe Inventory and Invariants

- Public init → backend::init: the input has been normalized and has no physical overlap; the caller guarantees exclusive ownership of RAM,
  and the mapping callback provides pointers only for the selected backing region. with_reserved_storage deducts inventory only after that initialization succeeds.
- Segment Table::initialize: indexes and counts come from build_segments over
  normalized ranges. Array size, isize limits and capacity are checked before
  writing slots. The unsafe contract carries the map callback's PageMeta alignment
  requirement through public init and backend::init to initialize. Slot offsets
  preserve alignment. PageMeta contains AtomicUsize, so mappings must meet the
  actual type alignment rather than the earlier byte-aligned representation.
  Table is constructed only after every descriptor is initialized; no pointer or
  reference exposes uninitialized slots.
- segment lookup: the segment boundaries and compact indexes of the segment table embedded in Table prove that the slot is initialized and within the array,
  and a shared borrow is returned; input normalization is performed only on the shared free_ranges path. covered_pages is
  derived from the last segment and does not depend on a separately stored count; all descriptor indexes and the array size were validated at construction.
- segment mark_permanent_range: first validate the complete physical range, then call PageMeta::mark_permanent exclusively;
  it does not run concurrently with any shared descriptor borrow.
- Table's unsafe Send/Sync: moving the table does not move its backing storage.
  Shared reference state uses atomic updates; permanent marking requires exclusive
  access before publication.
- publish: installs the metadata table through Once and allows a static borrow; the caller serializes publication and guarantees that the backing
  RAM and mapping remain valid permanently; a local test fixture with a limited lifetime must not be published.

## Lifetime and Errors

Initial descriptor values and permanent flags are written before publication.
Global Once provides visibility. Lookup takes no extra lock; reference-state
observations and transitions use per-page atomics. Dropping a local table does
not reclaim its backing; the caller may reclaim it only after all borrows end.
Global metadata lifetime is independent of allocator instances.

| Failure | Handling | Scope of guarantee |
| --- | --- | --- |
| Reversed start/end, address arithmetic overflow, no complete page | InvalidRange / EmptyCoverage | Hardware accessibility is not probed |
| Physical overlap, insufficient backend index capacity | Initialization returns an error | The input may already have been normalized; no metadata table is published |
| Insufficient backing RAM | NoMemory | The failed request is not deducted |
| Mapping callback or backing initialization fails | Returns the corresponding error | The request is not deducted; the callback is responsible for its external mapping resources |
| A later reservation fails among multiple future reservations | Returns an error | Earlier successful deductions are retained; no overall rollback is promised |
| Lookup hits a hole or the metadata table is unpublished | None | Does not mean that the physical page does not exist |
| Duplicate publication | AlreadyPublished | The caller must still serialize publication |
| Backing region is reclaimed too early or handed back to the allocator | Violates the unsafe contract | May cause UAF or corruption; the type does not own the raw RAM |

Normalization preserves the slice length; after merging, redundant entries and fully consumed ranges may be empty. Boot integration must skip
empty entries; only the remaining ranges after in-place updates are handed to the allocator. If allocator initialization fails after the metadata table is published,
boot stops, with no partial retry.

## Scope of Guarantees and Audit

Metadata backing never enters ordinary free inventory. kalloc raw release does
not scan each page's references or permanent flag; callers must own the original
allocation's release right. This service does not validate provenance, detect
raw-protocol double frees, or record allocator bitmap/heap/PCP ownership in
metadata. It parses no user input, retains no user-page contents and writes no
persistent data.

Audit focus: whether in-place list deduction and metadata coverage are distinguished; whether the mapping targets only the actual backing region and is large enough;
whether the backend hides the index geometry; whether publication happens after all initialization writes; whether the caller retains the backing mapping,
skips empty entries and hands over only the remaining RAM; whether local tests destroy fixtures in the correct order.


## Allocation references, grouping and unsafe callers

PageMeta fields are private. PageMetaTable::claim_reference_range and
claim_reference_group accept only
exclusive unpublished inventory and validates every descriptor before acquiring
any initial holds. Independent ranges keep order/offset zero on every member;
explicit compounds publish their first head hold only after membership setup. A permanent page, physical hole, nonzero count or existing tail/order
prevents admission. Ordinary error paths do not claim a prefix. The immutable
published directory and caller-owned backing establish descriptor lifetime.

Tail membership uses a checked u32 PFN distance. reference_allocation requires a
live hold from the same allocation incarnation, resolves a head with a positive
count, and checks the member fits its order. A live hold excludes retirement and
exclusive split, making geometry stable. Safe lookup, global_table and
ReferenceState cannot establish that hold; snapshots may mix concurrent phases.
They must not be used as release authority or as a lockless acquire protocol.

Clone and release operate only at heads. The counter limit excludes all three
transition sentinels. Initializing protects attribute/link publication and Release
publishes the first owner. Before calling release_reference, the caller must
end all accesses authorized by the consumed hold and wait for any required
TLB/device completion. Last release enters Retiring with AcqRel. The last
holder snapshots geometry before release; finish_reference_retirement clears all
tail membership, resets the head, and returns the original class before kalloc
publishes reusable inventory. No old descriptor writes may follow that return.
Per-field atomics alone do not replace this ownership and publication argument.

split_reference freezes exactly one live head hold with AcqRel; any outstanding
raw transfer still counts and blocks exclusivity. Errors preserve the original
holder. After freezing, all metadata is already known covered, no allocation or
fallible validation remains, and every child receives one independent hold.
Class copies and child publication precede exposing outputs. The unsafe caller
must end content access, mappings, devices and required retirement, consume the
old holder without Drop, and release every resulting child once. These are
ownership transfers, not reference acquisition from a diagnostic count of one.

kalloc's SplitPages iterator owns unconsumed children; its Drop prevents ordinary
early iteration exit from leaking them. mem::forget, a lost raw transfer or a
kernel failure can still leak ownership. Neither this crate nor a count check
proves provenance, unique raw consumption, allocation incarnation or hardware
retirement. Invalid unsafe callers can corrupt the allocator despite state checks.

Metadata stays 16 bytes on 64-bit targets by a compile-time assertion. Offset,
order, class and count use separate atomic fields; no tagged pointer or count/
offset union is decoded. The permanent flag is immutable after publication.
All UsageKind values come from typed admission; split preserves the original
class and final accounting remains allocator-owned. Group admission, retirement
and splitting touch every backing descriptor, so IRQ callers must bound their
work. Ordinary raw release remains unchanged and performs no descriptor scan.

## Independent ranges and clone rollback

Reference-range shape is either all independent pages or one compound covering
all selected pages. The caller retains an original hold for each relevant unit
through validation/clone; range diagnostics cannot acquire those holds. Independent
clone releases only its successfully acquired prefix on failure. Every original
hold remains retained, so rollback cannot select last retirement. Compound clone
changes one head count. Range length belongs to the external holder, not a shared
metadata field; raw consumers must transfer that length without reconstruction
from the first descriptor. Isolated fixture tests force mid-range overflow without
exposing a production count-setting API and restore all modeled holds afterward.
