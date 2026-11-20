# Host companion

This directory contains the host CA for the
[TIPC bidirectional large-transfer test](../README.md). It uses
`libtrusty-rs::TipcChannel` to reach the TA service through the QEMU guest's
vsock-TIPC bridge.

Build it from the repository root:

```sh
cargo build --release \
  --manifest-path uapps/tipc-bidilarge-test/host-companion/Cargo.toml
```

The first CLI argument is the guest CID, defaulting to `103`. The optional
second argument sets the CA upload chunk from 1 through 8192 bytes and defaults
to 4 KiB.

The client fails the run if CA-to-TA upload throughput is below 5 MB/s, which
catches a regression to the former fixed 10 ms bridge backoff.
