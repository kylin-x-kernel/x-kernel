# Platform Contract Boundaries

Providers own hardware and firmware operations. Boot phases retain IRQ masking
and once-per-CPU sequencing obligations. CPU startup accepts validated logical
IDs; translating them to hardware IDs belongs to the platform. Power terminals
do not perform filesystem/process/device cleanup.

PMU IRQ registration precedes activation. NMI source and mechanism protocols
retain their separate contexts. MMIO/DMA preparation errors must be handled by
their callers; moving or renaming contracts does not certify rollback on
partial platform sharing failure. The existing AArch64 partial-sharing error
policy is tracked separately in kiface's capability audit.
