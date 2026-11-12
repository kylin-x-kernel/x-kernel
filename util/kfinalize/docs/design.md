# kfinalize — Design Document

## Fallible Payload Reservation

After provider validation and carrier preparation, both constructors reserve
`KBox<MaybeUninit<Payload<T>>>` with `AllocFlags::NOWAIT`, before moving `T`.
Failure returns `(CreateError::NoMemory, T)` and drops the unused carrier;
it neither creates an identity nor submits finalization. `KBox::write` then
initializes the reserved address without allocating, and `KBox::into_raw` /
`KBox::from_raw` transfer and recover the unique payload owner. Weak expiry,
the raw identity header and NMI retirement protocols remain unchanged.

Normal construction still requires a sleepable task. It does not grant reclaim
permissions merely because sleeping is allowed: callers may hold locks needed
by reclaim or the finalizer. Integration construction can run during boot or
with preemption disabled, so it also uses NOWAIT. This only constrains payload
reservation, not nested allocations, provider preparation or payload Drop.

The carrier still uses `Arc::new`, and the identity uses `Arc::new_in` with the
provider's `Global.allocate` backing region. Both retain the existing fatal
kernel OOM policy; using `Arc::try_new_in` alone would not fix that backing
allocator. This batch does not change their NMI deallocation protocol or claim
whole-constructor OOM recovery. Task constructors still lack an admission-error
channel and treat finalizer preparation failure as fatal; their rollback/API
redesign remains separate from this payload migration.

Guest tests inject only the private payload reservation operation, verifying
unchanged input ownership, unused-carrier release, no submission and provider
error precedence. Existing raw-owner and host/Miri probes cover successful
publication, alignment, ZSTs, Weak expiry and modeled NMI retirement. This is
not global OOM injection or proof of all kernel NMI/reclaim interleavings.

## Positioning and Scope

The public `DeferredArc<T>` / `DeferredWeak<T>` separate the release of the last public strong reference from the destruction of the payload. Only ordinary sleepable worker destruction is provided; no BH destruction type is provided. Ordinary users depend only on kfinalize; this crate does not depend on ktask, kirq or kworkng. The safe constructors still require that the provider is installed and the current context can sleep; audited boot/task preparation prepares ahead of time through the unsafe service::new_integration. This entry point reuses the existing shared list to keep the unbound responsibility alive, and does not extend to Task/RCU migration or a public drain API.

The hidden unsafe service::new_integration entry obtains a fresh carrier and
shares the same identity/KBox construction. After primary-CPU installation,
independently initialized CPUs may prepare concurrently. NotReady and payload
NoMemory return the unchanged input. Early retirement expires Weak immediately;
the permanent inbox retains destruction until workers bind. Failed boot does
not guarantee reclamation, and startup must not wait on its own executor.

## Usage Principle: Do Not Use Unless Necessary

By default, keep the ordinary Arc and direct destruction. Consider DeferredArc only when there is an explicit requirement that "the last reference may be released in a context where this object is not allowed to be destroyed"; do not replace everything merely to unify types or to avoid writing callbacks.

- When adopting it, state the contexts in which the last reference may occur, why the complete destruction chain cannot run there, and how the ordinary worker context satisfies the requirements.
- Paths that already have a safe and correct direct release need no rework; when a BH callback after an RCU grace period can already destroy directly, do not wrap it in an additional deferred destruction type.
- Evaluate the number of objects, the creation/destruction frequency, the resident memory of the backlog, and the dependency on the shared worker; for large numbers of small objects in particular, the fixed per-object metadata must not be ignored.
- Asynchrony only provides execution-context conversion; it does not guarantee speed, nor lower tail latency; likewise it does not take on any RCU/device lifetime protocol beyond the reference.

It is still three allocations per object: an identity with an NMI retirement prefix, a payload with a raw-owner
header, and a carrier with an NMI node; a ZST also has a payload header allocation.
On 64-bit, the DeferredArc/DeferredWeak handles are 24 B each, while an
Arc/Weak from an ordinary global allocator is 8 B. Clone does not create a new carrier, but dereferencing involves an extra pointer indirection,
the last-reference release adds queue synchronization, notification and worker scheduling, and NMI additionally adds an ordinary IRQ handoff.
The current total allocator request volume, slab usage and overall Task increment have not been re-measured,
so measurements from historical versions must not be cited as the basis for current adoption. Large-scale adoption requires supplementary measurements for the target configuration.

The adoption review must record: the destruction-context conflict, the peak object count and creation/destruction frequency,
the per-object and backlog memory budgets, the wait dependencies of the shared executor,
and the module's own completion/shutdown protocol.
Type constraints and constructor checks cannot automatically prove these business conditions; if they are not satisfied, keep the ordinary Arc,
or first adjust the module lifecycle, rather than replacing first and then assuming the background can release in time.

## Architecture

```text
DeferredArc ── Arc<Identity<T>, IdentityAllocator> ── KBox<Payload<T>> (raw header + T)
                     │
                     └── Arc<dyn Prepared> ── lightweight list node ── shared Normal Work
DeferredWeak ── Weak<Identity<T>, IdentityAllocator>
```

`src/lib.rs` owns the public identity and the payload; `src/service.rs` is the hidden provider contract for the kernel integration layer. Early in startup, kworkng installs the integration preparation entry point; after the execution service initializes successfully, it installs the ordinary Normal preparation entry point; the two share the same executor, and each returns an independent carrier. Ordinary construction returns NotReady directly when the ordinary entry point is not installed, and does not bypass the context check just because the integration entry point is installed.

No business consumer was found that needs "automatic BH destruction on the last reference", so the first version removes the BH entry point. The existing non-sleeping destruction callbacks of RCU remain unchanged; they will be re-evaluated in the future only if a concrete consumer demonstrates an independent need. Not sleeping does not mean being safe in any context, but this distinction by itself is not a reason to add a new general-purpose type.

## Calling Constraints / Execution Context

| Operation | Allowed context and preconditions | Guarantees not provided |
| --- | --- | --- |
| Plain new | Sleepable task context with a provider installed | Ad hoc construction in IRQ/NMI is not allowed |
| unsafe service::new_integration | Provider installed, each CPU's local allocator/per-CPU/IRQ locks ready; audited boot/task preparation, preemption may be disabled | hardirq/softirq/NMI construction not allowed; not a general bypass of the checks |
| Last strong reference release | task, softirq, hardirq, audited boot paths; NMI requires this CPU's irq_work to be prepared | Lock-ordering constraints on ordinary paths are unchanged; wait-free is not guaranteed |
| Last Weak release | Ordinary context frees the identity directly; NMI hands off to ordinary IRQ via a separate irq_work node | NMI without a prepared irq_work is not supported; no latency guarantee |
| Payload Drop / KBox free | Sleepable normal worker, outside queue locks | Never inline in submit; no latency bound |

The NMI branch is decided before any ordinary guard, pending/queue lock or allocator free.
The original Arc owner of the payload carrier is handed off to ordinary IRQ, and afterwards enters the worker along the original list.
The control block uses the standard Arc's allocator_api: the provider prepares a separate
retirement header outside the requested region, and the allocator deallocate of the last Weak merely hands out the original allocation token.
The ordinary path
adds no IRQ hops. An unsupported or unready NMI handoff is treated as a fatal contract violation, and must not
fall back to releasing directly inside the NMI. The temporary owner obtained by Weak upgrade obeys the same conditions.

This does not amount to forbidding NMI from reading an object: if a module has a separate protocol that guarantees an owner outside the NMI
covers the entire borrow period, and the read operation itself is suitable for NMI, that borrow may be audited individually.
DeferredArc does not provide this borrowing protocol automatically, nor does it guarantee that the methods of an arbitrary T are suitable for NMI.

The provider checks context and readiness before payload reservation. Recoverable
failure returns the original input; only payload reservation gains NoMemory
recovery. Clone/Deref/Weak retain standard Arc semantics. NMI final Drop needs
initialized local irq_work. Submission allocates no carrier, never waits for
workers and never destroys the payload inline; it is not wait-free or safe
under arbitrary locks.

IdentityAllocator is an allocator that performs one allocation per identity, not a general multi-allocation pool.
Only after a successful allocation can it be cloned, copying the permanent policy and the original token; all copies refuse a second
allocate. These copies are produced only after the Arc has been constructed successfully, and destroying the allocator itself releases no
resources. The token separately preserves the original provenance, and the whole allocation cannot be inferred from the region pointer narrowed by Arc and then freed.
The public handles therefore contain 16 B of allocator state (64-bit).
The default Global policy is only for internal tests before installation; the production provider must install the memory
policy first, and early objects retain the policy at the time of creation, so the deallocator is not switched afterwards.

The final Normal Drop may sleep, but it must still avoid wait-dependency cycles in the shared pool. Even if the last reference is released in softirq, it is only submitted to an ordinary worker and not destroyed inside the softirq. Flushing to disk and stopping devices before shutdown are still guaranteed synchronously by the owning module; this mechanism is not a shutdown barrier.

The shared Work serializes current ordinary reclamation: if one Normal destruction sleeps, it blocks subsequent Normal nodes. Therefore a destruction must not directly or indirectly wait for the reclamation of other objects in the same executor to complete; enqueuing new objects is allowed, but waiting for them is not. This is a new adoption restriction introduced by the shared scheme, not a problem that additional threads can solve automatically.

## State Machine and Algorithm

1. Check the provider and prepare the carrier; any recoverable failure has not yet taken over the input.
2. Reserve and initialize stable KBox<Payload<T>> storage, construct the standard Arc identity, then initialize the raw header before publication. Identity, payload and carrier remain separate allocations, including the header for ZST values.
3. After the standard Arc drops the last strong reference to zero, Identity::drop is called. From this point all Weak upgrades fail.
4. Identity hands out the unique Finalization; the carrier keep-alive does not reference the public identity and does not extend the Weak upgrade window.
5. The prepared executor consumes Finalization; KBox::from_raw runs T::drop and releases payload storage. Submission context does not change the executor.

## Concurrency Model and Design Decisions

### Scheduling Ownership Interface

`as_ptr` only borrows the address of T; `ptr_eq` compares identity; `strong_count` is merely an instantaneous
observation and cannot replace synchronization. `into_raw/from_raw` hand off/restore one strong reference, without increasing or decreasing
the count and without allocating. Each raw owner must be restored exactly once; if omitted, the strong reference remains and is leaked;
a duplicate restoration is an unsafe contract violation by the caller. While a raw owner exists, a Weak can still be upgraded.
`borrow_raw` is dedicated to non-owning views such as current: it returns a ManuallyDrop wrapper,
which neither consumes the original count nor adds a reference. The caller guarantees that the real raw owner covers the view and its
borrows, and must not explicitly Drop/take out the handle inside the wrapper; only clone produces an independent owning reference.

The payload stores an immutable RawIdentity before T: the original Arc identity pointer and a compatible
IdentityAllocator copy. It is initialized before first publication and adds no keep-alive reference,
so it is not a reference cycle. into_raw projects T from the full payload raw pointer; from_raw accepts only an owning pointer
that retains the allocation provenance, and returns to the header by the repr(C) field offset.
The header cannot be derived backwards from a &T produced by Deref; as_ptr itself also does not grant a strong reference.
This does not conflict with the prohibition on recovering the allocator backing allocation from a narrowed Arc region pointer:
The retained pointer has the full KBox allocation provenance, not authority widened from a shared T reference.

After the last strong reference ends, the identity pointer in the header may dangle, and the worker never reads it;
the MaybeUninit header needs no destructor, and its allocator copy itself releases no resources. The worker only
The worker drops T and frees the whole payload KBox. Last Weak release retains the identity-memory protocol.

On 64-bit, RawIdentity is 24 B; a usize payload request goes from 8 B to 32 B, a ZST
goes from no payload allocation to 24 B, and other types are additionally affected by alignment. For non-ZST types the number of allocations
does not increase, and the DeferredArc/Weak handle size remains 24 B. The actual Task cost awaits adoption measurements;
host correctness tests must not be treated as formal performance data.

Races between public strong/weak references fully reuse the standard Arc/Weak. While public references exist, the payload only has shared access; only the unique token after the public strong references disappear can obtain the right to destroy. There is no home-grown reference count, and no scanning or retry.

The consuming Arc receiver of Prepared and the Arc held by the pending list protect the submitter and the executor respectively; the worker may finish the payload destruction first, and the submitter can still return safely. The public identity, the carrier and the payload are separate, so this race does not require the payload to remain until publication returns. The list lock only protects publication/removal; notifying the shared Work, destruction and freeing nodes all happen outside the lock, and at most 64 items are processed per round, with the remainder taken over by subsequent execution rounds.

## Drop / Resource Release

| Object | Release location |
| --- | --- |
| T destruction and KBox<Payload<T>> storage | Finalization::finish on a normal worker |
| Arc public control block | Caller of the last strong/weak reference; possibly on an IRQ path |
| Empty carrier allocation and metadata | Last publisher or worker; possibly on an IRQ path |

The last two rows must not contain business destruction that needs to sleep, and it is not claimed that all allocator frees are made asynchronous. This is a source-level ownership boundary, not the result of per-call allocator tracing measurements.

## Usage

Depend on `kfinalize.workspace = true`, and call `DeferredArc::new(value)` in a valid task context. Initialization is done by runtime/kworkng, and users need not create or manage a WorkHandle. There is no get_mut, try_unwrap, arbitrary queue selection, or interface to cancel the final destruction.

The RCU grace period remains the responsibility of RCU, and this crate does not change the RCU scheduling foundation. Objects that can already be safely released in a BH callback after the GP need not queue another BH just to use this type; destruction that needs to sleep can be handed off to Normal after the GP.
