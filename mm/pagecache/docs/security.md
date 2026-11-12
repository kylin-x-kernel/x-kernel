# pagecache — Security and Reliability Analysis

## Trust Model

`pagecache` trusts the owning `kvfs::AddressSpace` to provide an already validated and serialized index, range, and
visible `i_size`. It performs no access control, and it owns neither the file length nor the VM lifecycle.

## External Boundaries / Attack Surface

- folio index and in-page offset;
- old/new length provided by the owner;
- writeback range and visible length;
- failure results of the materialize/writeback closures.

## unsafe Code Inventory

All unsafe page-memory operations are concentrated in the private `src/folio.rs`; its fields are not exposed to other modules.

1. `Folio::new_zeroed(index)`: zeroes the exclusively writable page returned by the page allocator.
2. `Folio::data()` allows only the exclusive owner of an unpublished page to
   borrow its data. Published pages are accessed through `FolioGuard::data()` or
   `FolioRead::data()` under the stable page object's own lock.
3. `PinnedFolio::copy_to_uninit()`: locks the same stable page object held by the pin and copies the full initialized page. Cache detachment or replacement does not change its allocation or lock, so the original page can still be copied; the destination is bounded by an exclusive reference to a fixed single-page array, `&mut [MaybeUninit<u8>; PAGE_SIZE_4K]`.

## Memory-Safety Invariants

- On a synchronous writeback error, budget updates of completed pages are retained before returning, and all started folio writebacks are ended;
- Submitted writeback transfers completion responsibility, and the callback returning does not clear writeback; the owner must bound in-flight batches,
  and complete or discard the completion in task context after all I/O ends, and must not acquire the cache mutex in IRQ/BH;
- Cross-task completion retains the complete mapping owner. Conversion validates
  embedded-index identity, and the owner's projection must remain stable. Release
  the owner only after completion or Drop settles its folios; never extend index
  mutex lifetime independently.
- Each `Folio` owns exactly one page;
- `FolioBatch` holds only bounded references in traversal order; a live reference does not mean the mapping identity is still valid;
- batch clear/drop does not end writeback; any started writeback must be completed explicitly first, including failure suffixes;
- the traversal candidates and the batch awaiting completion are each at most 31 pages, and contiguous writeback data is at most 31 pages; temporary memory does not
  grow with the whole dirty range or the truncated page count; the total in-flight memory of submitted writeback is limited separately by the caller;
- a page index is associated with at most one cached folio; before publication, the tree key is checked to match the folio's immutable index;
- Folio bytes and ordinary state are accessed only under the folio lock; waiters may inspect the atomic writeback bit without that lock;
- pinned-folio copy-to-uninit reads the fully initialized page under the lock of the stable page object itself;
  on success it returns only after the MaybeUninit target is fully written;
- Explicit removal holds the folio lock before invalidating and unlinking.
  Exclusive final-container destruction invalidates atomically without waiting;
  retained tokens keep storage alive. Normal and batch lookup reject detached
  identities, and a successful access callback is never replayed;
- the batched-read callback only reads data and updates readahead marks, and does not modify dirty or residency state;
  index queries are allowed, but it must not wait for other folios, and readahead I/O must run after the current folio lock is released;
- the owner must make the backing read and publication exclude truncate, and bound the copy by the current EOF;
- `Folio::dirty` is the source of truth, and the dirty index only provides `PAGECACHE_TAG_DIRTY`-style candidate enumeration;
- `Folio::writeback` is the source of truth, and the writeback index only provides wait enumeration;
- when an external folio operation returns, the dirty-state change is already reflected in the dirty tag of the current resident folio;
  an index removed from the resident tree must not remain in the dirty index;
- a dirty tag hit is not permission to write back; before starting I/O, the mapping identity, dirty and writeback
  state must be rechecked in the same critical section of folio lock → MappingPages state lock;
- completion only takes the index lock, rechecks identity and updates atomic
  dirty/WRITEBACK state; it never acquires the folio content lock. Failure only
  redirties the current resident; detached completion only ends its WRITEBACK;
- clear WRITEBACK and its mark before releasing the index lock, then wake;
- `MappingPages` has no length field that can diverge from the inode `i_size`;
- VM object-id, views and notifiers do not enter this crate.

## Thread Safety

The folio tree/dirty index and the folio data are locked in layers. When both layers of state must be checked together, the lock order is fixed as
folio lock → MappingPages state lock; under the index lock only a non-blocking try_lock is allowed, and on failure the index lock must be released
before waiting again. The folio access closure completes dirty index reconciliation while holding the page lock. Filesystem I/O for writeback and buffered page-fault reads runs outside the tree lock.

The pause-only spin on index mutex contention has a fixed budget; after the budget is exhausted, blocking and wakeup still go through the existing wait queue;
preemption is not disabled, and the index critical section is not turned into a non-sleepable region. Longer index updates,
residency admission and the owner being scheduled away remain subject to the mutex's existing execution-context and progress constraints.
The buffered-read owner holds the shared invalidate lock to coordinate EOF,
page reservation and submission. The transferred folio lock makes truncate wait
for completion before clearing the tail or detaching the page; completion itself
does not reinsert the page into the cache. The folio enters the resident tree
before reading, and I/O retains the folio lock without holding the tree lock.
Only fully initialized pages may be marked uptodate, and a failed page exposes no partial data. After waiting for the page lock the resident folio is looked up again,
and references that left the cache during the wait are not used. The read-in callback must not re-enter the mapping or acquire locks needed by truncate.
`writeback_until` calls the backend callback under the folio lock;
that callback must not re-enter the same `MappingPages`; batch writeback releases the folio lock before calling the backend,
but the callback must not wait for or truncate folios of its own submitted batch, or the completion path cannot run.

## Threat Analysis

| ID | Threat | Control |
|------|------|------|
| T-01 | In-page offset/length out of bounds | checked arithmetic and page-size boundary checks |
| T-02 | Tail page leaks old data after truncate | shrink zeroes the surviving tail |
| T-03 | writeback fails but the current folio's dirty is cleared | the failure path restores dirty and ends writeback after identity validation |
| T-04 | readahead overwrites a foreground dirty folio | insert-if-absent; the already cached folio wins |
| T-05 | the cache layer forms a second EOF/object owner | does not store length, object-id, views, or notifier |
| T-06 | a missing dirty index entry causes fsync to miss a write | reconciliation on folio dirty state transitions; resize/writeback explicitly synchronize the index |
| T-07 | writeback completion overwrites a concurrent redirty | the batch completion phase rereads the final state; after single-page writeback, a clean → dirty writer sets the tag again |
| T-08 | writeback of an old folio already detached from the mapping after truncate/replace | at both writeback start and completion, the current `Arc` identity is validated under the fixed lock order; on failure the old folio is not re-marked dirty |
| T-10 | collecting references for a whole large mapping during writeback or truncate exhausts temporary memory | fixed-capacity FolioBatch; the dirty cursor advances; the batch is reused after deletion |
| T-09 | the zeroed result of the cached tail after file growth does not enter writeback | growth sets the dirty bit and dirty tag together under folio lock → state lock |

## Failure Mode and Effects Analysis (FMEA)

| Failure Mode | Current Handling | Impact |
|----------|----------|------|
| folio allocation failure | returns `NoMemory` | the current I/O/fault fails |
| range arithmetic overflow | returns `InvalidInput` | the current operation is rejected |
| backing write failure | keeps dirty | can be retried later |
| dirty tag points to a stale instance or a clean folio | rechecks the current resident folio under the fixed lock order and corrects the tag | does not call the backend and does not pollute the replacement folio |
| cached tail zeroed after file growth but not persisted | growth sets the dirty bit and dirty tag together | later writeback will cover the newly visible range |
| final teardown has a dirty folio | explicitly clears dirty and then releases | the inode has already left its lifecycle |

## Known Limitations

- The resident folio tree and the dirty index use `BTreeMap`/`BTreeSet` respectively; the scan of all resident folios
  by writeback has been eliminated, but the Linux XArray tag/reclaim is not yet implemented;
- While truncate waits for writeback it releases the mapping/folio locks, and after reacquiring them it rechecks identity and writeback;
  the owner must still serialize size changes and forbid refilling pages within the truncated range;
- `writeback_until` calls the backend while holding the folio lock, and a callback that reenters the same `MappingPages` causes a
  deadlock;
- There is no adaptive readahead, swap, memcg, NUMA, or THP;
- inode/MM consistency is guaranteed by the transactions of `kvfs::AddressSpace` and is not reimplemented in this crate.

## Audit Checklist

- Whether length, object-id, view, or invalidation state has been reintroduced;
- Whether the owner passes the current inode `i_size` when performing writeback;
- Whether growth sets the dirty bit and dirty tag after zeroing the cached tail;
- Whether shrink zeroes the partial tail and deletes the whole pages after EOF together with their dirty tags;
- Whether writeback failure keeps dirty;
- Whether the dirty index is updated before a dirty state change returns, and whether the index is still retained after a concurrent redirty;
- Whether dirty tag candidates revalidate the current mapping identity and folio state before writeback starts;
- Whether shrink and final teardown synchronously delete the corresponding dirty index;
- Whether the MappingPages/folio two-lock paths keep the folio → MappingPages state order, and whether only a non-blocking try_lock is allowed under the index lock;
- Whether the `writeback_until` backend callback avoids reentering the same `MappingPages`;
- Whether writeback calls the backend outside the tree lock, and whether the materialize closure avoids reentrancy;
- Whether `MappingPagesState` stays visible only within the crate, and whether the container is always held directly by AddressSpace.

Setting and consuming the readahead mark requires exclusive folio access; the mark affects only performance policy and cannot replace the uptodate
or resident identity checks. A cache-history query returns a transient hint and neither extends the residency period nor authorizes publication.

## Stable folio lifetime

- `Arc<Folio>` unifies physical ownership, immutable identity and the lock that
  protects every safe content access. Public callbacks receive only a
  `FolioGuard`; its fields are private and it cannot yield or replace the owner.
- Pins refer only to this object. A replaced index cannot redirect their lock
  or accidentally retain the replacement page. The physical address is derived
  from this object's stable allocation.
- `PinnedFolio::map_readonly` consumes the pin and returns a must-use
  `MappedFolio` token. The unsafe publication contract requires EOF validation
  under the invalidate guard and retention of the returned token through PTE
  removal and completed TLB invalidation. Clone tokens share the same object.
- WRITE, DEVICE, UNCACHED and SHARED attributes are rejected before publication.
  Existing raw physical-address consumers are migrated in follow-up consumers;
  this provider change does not authorize writable user aliases.
- Copying takes a fixed single-page array, so an incorrect length cannot reach a
  kernel panic. A retained detached page remains copyable, serialized with that object's
  writers, and initializes the full output. It neither proves current EOF nor
  promises an immutable snapshot. Buffered resident traversal instead rejects
  invalidated entries because it promises current cache data.
- Ordinary shared ownership retains pages without a separate pin counter or registry.

## Cache residency lifecycle

The filesystem owns capacity policy and supplies one `CacheResidency` provider
when constructing an inode mapping. Only a winning publication calls
`try_add_folio()` under folio and index locks; its charge is attached to the
stable folio. Detachment, truncate and mapping destruction cannot release quota
while pins, tokens or batches retain that physical page. The last reference
frees the allocation before returning its single charge, with neither lock
held. Failed admissions and losing allocations own no charge; failed reads keep
the resident reservation charged. A charge retains its provider past mapping
destruction, so delayed release cannot use a destroyed accounting object.
Providers must not reenter mappings, acquire folio locks, or perform I/O.
Release and final provider destruction must not sleep: the last reference can
be released by I/O completion. Admission still requires task context.
For memfs, the provider is its filesystem-owned quota, so lock order is cache
folio -> index -> quota limit; remount and statfs never acquire cache locks.
Speculative physical storage remains outside the admission policy until insertion.


Resident batch traversal reports absent identities only from its bounded index
snapshot. An existing non-uptodate or invalidated candidate is resolved again
before exposing data; initialization errors are preserved. The owner holds
invalidation shared through the batch, including EOF checks and user copies.
Only one folio is locked at a time; callbacks cannot reenter this mapping or
wait for another folio. An initialized hit takes no additional index lock.

Writeback completion borrows the index and retains the original folio identities.
The filesystem separately prevents truncate or eviction from releasing backing
blocks before I/O finishes. Completion accepts only a prefix within its snapshot;
an invalid prefix redirties the batch, and success preserves concurrent dirtying.
Writeback completion never takes the folio content lock: it reconciles marks
and clears the writeback bit under the index mutex, then wakes waiters after
releasing that mutex. A tag scan cannot pass a removed mark before the
corresponding bit is cleared. A concurrent dirty update performs its own index
reconciliation. Completion is independent of the submitting task but still
requires sleepable task context for the index mutex. Detached identities are
never redirtied or reinserted.

## Initialization completion safety

`Folio` is Sync because a successful atomic lock acquisition uniquely grants
access to its UnsafeCell state and page bytes. A borrowed guard can become one
owned `FolioRead` without unlocking; the guard is consumed and never reused.
Both token forms release the same lock with Release ordering. `FolioRead` is
not cloneable, retains the allocation, and lends bytes only through `&mut self`.
Completion consumes it, so safe callers cannot complete twice or retain a byte
borrow across unlock. DMA owners must retain the token until device access ends.
Its `BioMemory` implementation exposes the same stable page for both byte views;
consuming typed recovery from a Bio returns the original lock token. Completion
must follow DMA retirement, including any device cache synchronization.
Splitting consumes ownership and partitions the page into non-overlapping
`FolioReadPart` ranges. Their byte views are constructed directly for those
ranges, never through a whole-page mutable slice while other parts are in DMA.
All parts retain the original lock and allocation through one shared completion.
Arc's final Release/Acquire destruction orders each part's data writes and
successful-byte accounting before whole-page publication and unlock. Failure
or abandonment cannot credit bytes; even a synchronous first completion cannot
unlock while unsubmitted parts remain owned.
Failure/drop leaves uptodate false. Waiters set the atomic hint and recheck the
predicate under their IRQ-safe bucket lock; wakeup matches both folio and bit.
Pinned stack entries remain borrowed until a scoped registration unlinks them.
Handoff first acquires the folio lock bit, then publishes granted ownership with
Release ordering; the receiving waiter uses Acquire and must not reacquire the
bit. Ordinary task wake hints grant no access. Killable demand reads serialize
cancellation with wakeup by unlinking under the bucket lock, and accept an
already published grant. Other lifecycle waits remain uninterruptible. Borrowed
notifiers remain live until that cleanup completes; no callback can retain them.
The waiters hint may remain set after exclusive wake, but cannot be cleared
while unvisited entries may still wait on this folio.
Read completion neither takes the mapping index lock nor sleeps. Final index Drop
atomically invalidates pages and clears dirty without waiting for initialization.
The read token retains allocation, charge and completion resources; freeing the
container cannot free a page still in use by I/O. Dirty updates cannot resurrect
an invalidated page. Owners must explicitly drain before releasing any resource
not retained by their I/O owner. Context admission errors precede mandatory
resize/writeback mutation; callbacks must restore the admitted task context.


## Bounded wait selection

Finite-target capture uses one logarithmic resident-tree endpoint lookup under
the existing index mutex. Clean old WRITEBACK cannot disappear merely because
its dirty mark was cleared at submission. The caller must use the same captured
range for submission and waiting and still protect detached filesystem-owned
work until required completion. Saturating endpoint arithmetic plus clipping
preserves the full u64::MAX sentinel without overflow. The target is local to
one operation, never an EOF authority or promise that every new write is covered.
No index/content lock is held across completion waits; no fsync intent or new
persistent coordination state is introduced.

## Shared selection and attempted-work accounting

TOWRITE is a derived selection index. Handoff clears the selected old identity's
mark together with dirty-to-WRITEBACK; removal discards it. A pass never clears
the shared selection on return. NONE skips busy WRITEBACK without counting a
filesystem decline; ALL flushes unsent batches before sleeping and revalidates
after waking. The same mutable control is borrowed only during submission.
Completion cannot retain or refund its budget. A declined unsent NONE batch
must restore dirty/WRITEBACK synchronously under existing producer exclusion.
Live EOF is borrowed owner metadata sampled under the folio lock; no second
size or per-folio generation is stored. Partial EOF ends a payload so growth
cannot concatenate bytes across a file-offset gap.
