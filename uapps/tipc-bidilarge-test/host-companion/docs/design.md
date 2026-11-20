# host-companion — Design

## Purpose and scope

This host-side CA drives the 10 MiB bidirectional transfer against
`tipc-bidi-large-server`. It is a test tool, not a library or production client.

## Flow

1. Connect to `VSOCK:<cid>:0` for service `com.xkernel.test.tipc.bidi-large`;
   the CID defaults to `103`.
2. Send `START_UPLOAD`, wait for `UPLOAD_READY`, then send 10 MiB in 4 KiB
   chunks by default. Chunk sizes are limited to `1..=8192` bytes.
3. Measure upload throughput through `UPLOAD_DONE`; fail below 5 MB/s to catch
   a regression to the old 10 ms bridge backoff.
4. Send `START_DOWNLOAD`, receive exactly 10 MiB, then send `DOWNLOAD_DONE`
   and require `FINISHED`.

## Execution context and lifecycle

The CA is a single-threaded standard-library process with one `TipcChannel`.
Arguments and control bytes are validated before use. Errors propagate as
`io::Error`; the channel closes on drop. No worker threads or `unsafe` code are
used.
