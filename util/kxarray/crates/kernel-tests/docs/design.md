# Integration Boundary

The unittest entry selects `kfeat/kernel-tests`, which optionally links this
crate and the container. Weak SMP forwarding does not enable the test crate in
ordinary system configurations. This crate depends on the XArray
container and the task/IRQ/RCU services being exercised. Neither `kxarray` nor
`kirq` depends on this crate, so a production `kirq -> kxarray` edge cannot create
a reverse path through test dependencies.

This split specifically prevents `kirq -> kxarray -> kirq` (directly or via
`ktask`) when KIRQ adopts XArray. It is not a general rule to extract kernel
tests into crates. Tests without the reverse dependency remain in the container;
needing scheduler services alone does not establish that a split is necessary.

`ut-tests/tasks.rs` owns competing writers and held-reader retirement fixtures.
`context.rs` covers hardirq, softirq and controlled NMI access.
`migration.rs` checks reader and prepared-writer migration under SMP.
All modules use the standard unittest registration macro. Their temporary state
belongs to each fixture; the runtime supplies only its normal initialized services.

The container's unittest-only helpers fix preparation/publication windows without
adding production API or runtime initialization. Host storage tests remain in the
container crate and do not require this integration environment.
