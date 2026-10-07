# hwrng — Security and reliability

## Trust model and external boundaries

Hardware bytes are untrusted input. This crate does not touch MMIO, user
pointers or DMA, and provides no entropy-quality guarantee. Consumers decide
whether to credit returned bytes as entropy. Implementations must validate
device lengths before copying into caller-owned buffers.

## Memory and thread safety

The contract contains no unsafe code. Drivers serialize consumption so concurrent
readers cannot consume the same bytes twice. In-flight DMA uses driver-owned
storage retained across wait errors. Host completion is Send + Sync and may
be notified in IRQ context without allocation or sleeping.

## Threats and failure handling

| Failure | Effect | Required control |
| --- | --- | --- |
| Notification races with sleep | Reader waits indefinitely | Register before predicate; preserve wake hint across park |
| Malformed device length | Buffer overrun | Driver validates length; successful count cannot exceed buffer |
| Device never completes and reports no fault | Reader remains asleep | No timeout or task-interruption exit exists in this contract; hardware progress needs separate monitoring |

`DriverError::InvalidInput` and `NoMemory` can arise from the wait provider
without a hardware fault. Since the shared error no longer records its origin,
a consumer must use device state or other evidence before permanently
disabling the source. A wait error does not cancel an in-flight DMA request.
No random bytes are logged by the contract. Source health tests, DMA teardown,
and malicious-device isolation remain outside this crate.

## Audit checklist

Check caller-buffer lifetime, exclusive consumption, wait-error cleanup,
notification outside device locks, and task-context validation in each provider.
