# memfd-test — Security and Reliability Analysis

## Trust Boundary

The program only sends a fixed, NUL-terminated label to the Linux ABI `memfd_create`, and then operates on the fd
it obtained. `/../buffer` is a debugging label and is not passed as a path to interfaces such as `open` or `unlink`.
It does not read user files, credentials, or the network, and does not modify shared filesystem nodes or global kernel settings.

## Memory and Lifetime

The mapping is accessed only after `mmap` succeeds, and the write length is less than 4096 bytes; the target of `pread` is
a fully sized fixed-length array on the stack, and the contents are compared only after exactly the expected bytes have been read. Short reads, errors, and content
mismatches all fail. Read operations are retried only on `EINTR`, and other errors are not ignored.
On both success and failure, the program attempts to unmap this process's mapping and close the fd; `close` is not blindly retried.

## Limitations and Audit

The program has no Rust unsafe code, shared state, or concurrency assumptions; the C mapping accesses are protected by the successful return value and the length
bound. It checks externally visible creation, mapped reads and writes, and content isolation, but cannot prove the private mount
topology, peak memory lifetime, performance benefits, or the complete semantics of sealing. Installation is not performed automatically,
and a pass on Linux cannot substitute for evidence from actually running on an X-Kernel guest.
