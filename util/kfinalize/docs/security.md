# kfinalize — Security and Reliability Analysis

## Payload Allocation Failure Boundary

Payload storage is reserved with NOWAIT after provider preparation and before
moving the input. Reservation failure returns NoMemory with the original T,
releases the unused carrier, and publishes no identity or Finalization. A private
fault-injection test verifies this rollback and that provider rejection prevents
reservation. These tests do not force global allocator OOM.

Only KBox::into_raw pointers are paired with KBox::from_raw. Header initialization
precedes public access, and the worker never reads the retired identity header.
The carrier and identity allocations still follow kernel fatal-OOM policy;
task-owner preparation still has no recoverable allocation-error channel. NOWAIT
does not make provider code, metadata release or T::drop NMI-safe, nor relax the
existing sleepable/integration construction requirements.

## Trust Model and External Boundaries

The input is a kernel Rust object T: Send + Sync + 'static; it does not directly accept user pointers, MMIO, DMA, firmware or network bytes. If T contains raw hardware/RCU accesses, the owning module must first end those accesses that are not counted as references; a reference count reaching zero cannot substitute for a GP or for stopping the device. The provider is trusted kernel integration code, not an executor that users can inject.

## unsafe Inventory and Memory-Safety Invariants

- `lib.rs::Identity` Send/Sync: the standard Arc guarantees the shared-access phase; T's Send/Sync permits cross-CPU sharing and the final handoff.
- `DeferredArc::deref`: public strong ownership protects initialized stable KBox storage; no mutable reference is exposed.
- `DeferredArc::from_raw/borrow_raw`: the original allocation provenance, the unconsumed strong reference, and the payload header that is immutable after initialization together guarantee recovery; a borrow_raw view must not consume the count, and the real owner must cover all borrows.
- `Identity::drop`: committed only once, with all public payload borrows ended. The pairing of pointer and destructor types is maintained by the constructor.
- `destroy_value`: the unique Finalization recovers the original KBox owner, dropping and freeing it exactly once.
- `service::Finalization::finish`: the caller guarantees an ordinary sleepable worker context and that it does not run inside the publish call. The token is not cloneable; dropping it only leaks and never wrongly runs the destructor inline.
- `service::Prepared` / `install_normal`: the unsafe provider is responsible for a single handoff, permanent service, keep-alive across publish/execution, no cancellation, and no allocation or sleeping at submission. Installation is for serial boot only.
- `service::new_integration` / `install_integration`: the primary CPU installs the permanent provider serially; each CPU may then prepare concurrently once it independently satisfies the local allocation/lock preconditions, including audited NoPreempt paths. The provider returns a fresh carrier and keeps responsibility while unbound; it creates no task and does not wait for execution. If not installed, the original input is returned. This unsafe entry does not allow hardirq/softirq/NMI construction; the ordinary safe construction checks are unchanged.

## Thread Safety

The public identity is not kept alive by the background, and a Weak cannot resurrect a retired object. The end of T's shared access synchronizes with the last Drop of the standard Arc; the queue is responsible for synchronizing the write of the token to the worker. The carrier is kept alive separately, and one cannot infer from the payload that the carrier is still alive, or vice versa. The core does not expose a Finalization constructor or mutable access to the payload.

## Threat Analysis and Fault Management

| Risk | Impact | Protection and Residual Constraints |
| --- | --- | --- |
| Construction in the wrong context | Original input destroyed early | Returns (CreateError, T); the caller handles the original value |
| Weak resurrected while queued | UAF/double free | Public Arc and private carrier are separate |
| Worker returns before submission completes | Carrier UAF | The provider must retain an independent publisher owner |
| Wrong provider executes inline | Sleeping in atomic context | unsafe installation and the Prepared contract; ordinary users cannot choose the provider |
| Clone/Weak changes the execution policy | Destruction in the wrong context | The original carrier pins the queue; a Weak upgrade still returns only a DeferredArc |
| Payload reservation OOM | Construction rejected | Return NoMemory and unchanged input; drop unused carrier without submitting |
| Carrier/identity OOM or impossible admission failure | Fatal stop/leak | Existing kernel policy; never free a token whose publication may have succeeded |
| Destructor panic | Leak or kernel stop | The destructor is not retried and no recovery is promised |
| Business waits form a shared pool or a wait cycle on the same finalizer | No progress | Each class of shared Work is serialized; a destructor must not wait for reclamation of other objects of the same class; users audit indirect dependencies, and this layer does no global deadlock detection |

## Privacy and Limitations

NMI support covers both the carrier handoff of the last strong reference and the control-block release of the last Weak.
The two have independent preparation nodes and independent lifetimes; NMI must not enter ordinary locks or the allocator.
The original allocation token is stored independently in the allocator copy, and it is forbidden to enlarge
permissions from the Arc region pointer to access/free the prefix. The provider must install the permanent memory policy before preparing objects.
NMI handoff requires this CPU's irq_work to be initialized; on failure it retains the original token and then reports a fatal error,
without destructing inline or degrading to the ordinary commit path. Accessing T in NMI still requires that the operation itself be legal.

Payload contents are not logged; business objects may be retained until the worker has a chance to run. It provides no upper bound on reclamation delay, no NMI reclamation on unsupported platforms, no making all frees asynchronous, no forced-shutdown draining, and no reference-cycle collection.

## Audit Checklist

The handoff of raw ownership adds a payload-header audit point: it is fully initialized before publication and read-only
afterward; it stores a non-owning identity pointer and an allocator copy that performs no release. The raw
owner itself retains an Arc strong reference, which guarantees that the identity is still alive at recovery. Only offset round trips
that preserve full provenance from the original into_raw pointer are allowed; constructing an owning pointer from &T or reading the header is forbidden. The number of recoveries strictly matches the number of handoffs one to one, and repeated
consumption is not allowed. The final worker does not read identity fields that may be dangling. Miri tests cover
intrusive field offset round trips, interleaved shared T accesses, high alignment, ZST, and release after NMI recovery.

- Do the last strong reference and Weak upgrade still all go through the standard Arc?
- Does the token come only from a public identity that has already terminated, and do the destructor and the allocation type match?
- Does the provider prepare only once and commit once, and preserve the execution context and storage during publication?
- Does metadata destruction still have no business sleeping behavior?
- Does a construction failure return the original input rather than implicitly dropping it?
