# tipc-bidilarge-test — Design

## Purpose and scope

This standalone TA measures 10 MiB RX followed by 10 MiB TX over the
vsock-TIPC bridge. It is a throughput test, not an integrity test: payload bytes
are counted, while control messages distinguish protocol phases.

## Architecture

```text
host CA -> VSOCK:<cid>:0 -> knet vsock-TIPC bridge -> TIPC service
```

The TA publishes `com.xkernel.test.tipc.bidi-large` with three receive slots,
8 KiB per slot, and 4 KiB data chunks. The host CA is in `host-companion/`.
`docs/vsock-tipc-throughput-analysis.md` records the path analysis and results.

## Protocol state machine

`Phase` is stored per connection:

```text
WaitingForUpload -> Receiving -> WaitingForDownload
                 -> WaitingForDownloadDone -> Complete
```

`START_UPLOAD` enables RX accounting. After exactly 10 MiB arrives, the TA sends
`UPLOAD_DONE` and waits for `START_DOWNLOAD`. It then sends 10 MiB in 4 KiB
messages and reports TX after `DOWNLOAD_DONE`.

RX timing starts at the first data message and ends when 10 MiB has arrived. TX
timing covers the contiguous send loop through the CA's completion notification.

## Execution context and lifecycle

The TA is a single-threaded `rust_libtipc` event loop. Service callbacks run in
that thread and may block on TIPC operations; there are no worker threads,
`unsafe` blocks, or FFI. The manager owns the port and connection handles. An
unexpected control message closes the connection; an event-loop error exits the
process with a non-zero status.
