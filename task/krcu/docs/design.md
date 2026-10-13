# Reader Accounting

`krcu` owns task-reader nesting without depending on the scheduler or IRQ core.
`ktask::TaskInner` embeds one `TaskReader` per task. Scheduler context checks
inspect it independently of the preemption-disable count: explicit read scopes
prohibit voluntary blocking but do not disable involuntary preemption.

Scope entry increments nesting, an internally owned guard covers the callback,
and guard destruction decrements nesting. A higher-ranked callback receives only
a borrowed token. Nested scopes remain active until the outer scope exits.
The token is neither Send nor Sync; no public constructor gives ownership to a
caller who could suppress its destructor. The callback can return copied values,
owned values or errors, but cannot return its token borrow.

The counter uses checked atomic RMW through shared references while permitting
scheduler observation, rejecting overflow/underflow without changing state.
Entry is AcqRel, exit is Release and observation is Acquire. This is local accounting,
not a proof of grace-period ordering. Unenrolled scope entry/exit takes no locks
and changes no IRQ mask or preemption state. No reader operation allocates.

## Preempted Readers

Each CPU owns a `ReaderNode` with an IRQ-safe locked intrusive list. Scheduling
an active task enrolls its embedded link before handing execution to another
task, including the same-task selection and migration-helper paths. Repeated
enrollment is a no-op only for serial calls: the host must exclude concurrent
enrollment and scope execution by another task or interrupt. A migrated reader
keeps its original node; there is no cross-node transfer or second registration.

The outer exit removes the link under its original node's lock and clears the
owner pointer. Nested exits do not touch the list. The node lock is followed by
a SeqCst fence, including on UP; there is no global list lock. The host excludes
ordinary IRQs/preemption during enrollment and retains the task allocation
until removal. `ktask` GC permanently retains an exited task with residual
participation rather than moving/freeing its embedded node or repeatedly retrying
the same task. Retention includes the entire task and all resources still owned
by its fields, not only its embedded reader or stack. Normal finite scopes
unlink before task exit and GC.

Only controlled task-context tests currently enter scopes. There is no public
current-task RCU read interface, protected-data container, grace-period engine
or reclamation service. The task-only removal path is not usable from NMI;
interrupt-context scope admission and deferred cleanup are not implemented.
