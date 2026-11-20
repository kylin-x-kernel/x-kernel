# TIPC bidirectional large-transfer test

This uapp contains both ends of a 10 MiB bidirectional transfer test over the
X-Kernel vsock-TIPC bridge:

- [`src/`](src) is the TA. It receives 10 MiB from the host companion and then
  sends 10 MiB back.
- [`host-companion/`](host-companion) is the host CA. It connects through
  `VSOCK:<cid>:0` and drives the same sequence.
- [`docs/`](docs) contains the throughput analysis.

The TA prints RX and TX throughput. RX starts with the first data message and
ends when 10 MiB has arrived. TX starts before the contiguous guest-to-host
send and ends when the CA's completion notification arrives.

This is a transport throughput test, not an integrity test. The programs count
bytes and use distinct control messages, but do not verify every payload byte.
The bridge forwards each vsock RX event as one TIPC message, so the TA's message
count depends on Linux vhost-vsock packetization and is not an application
message count.

## Protocol

1. CA sends `START_UPLOAD`; TA replies `UPLOAD_READY`.
2. CA sends 10 MiB in 4 KiB writes by default; TA reports RX and sends
   `UPLOAD_DONE`.
3. CA sends `START_DOWNLOAD`; TA sends 10 MiB in 4 KiB messages.
4. CA sends `DOWNLOAD_DONE`; TA reports TX and sends `FINISHED`.

The current buffer sizes are:

- virtio-vsock RX buffer: 8 KiB, including the 44-byte packet header, so one
  RX event carries at most 8148 payload bytes;
- TIPC maximum receive-buffer size: 8 KiB (`IPC_CHAN_MAX_BUF_SIZE`);
- TA TIPC receive slot: 8 KiB;
- connection receive window: 256 KiB;
- CA default upload write and TA download chunk: 4 KiB.

Linux vhost-vsock limits one packet to 64 KiB of payload. The test keeps both
application directions below that limit. The host client also fails the run if
CA-to-TA upload throughput is below 5 MB/s.

## Build and run

Run these commands from the repository root:

```sh
cp platforms/kplat-aarch64/qemu_defconfig .config
make defconfig
make build
make uapps UAPPS=tipc-bidilarge-test
make justrun ACCEL=n
```

`make uapps` modifies an existing `disk.img`; create or refresh the base image
first if it is absent. `ACCEL=n` selects QEMU TCG and keeps the measurement
reproducible on hosts without nested KVM.

In the guest shell:

```sh
/tipc-bidi-large-server
```

Wait for the ready message, then run the host companion from another terminal:

```sh
cargo build --release \
  --manifest-path uapps/tipc-bidilarge-test/host-companion/Cargo.toml

timeout 180s \
  uapps/tipc-bidilarge-test/host-companion/target/release/tipc-bidi-large-client \
  103
```

`103` is the default guest CID. Replace it when the QEMU command uses a
different CID.

The CA accepts an optional upload chunk size from 1 through 8192 bytes. This
allows a same-kernel comparison against the former 4052-byte packet-payload
size:

```sh
timeout 180s \
  uapps/tipc-bidilarge-test/host-companion/target/release/tipc-bidi-large-client \
  103 4052
```

The first run should report:

```text
tipc-bidi-large-server: ready (com.xkernel.test.tipc.bidi-large)
tipc-bidi-large-server: RX 10485760 bytes, ... messages, ... MB/s
tipc-bidi-large-server: TX 10485760 bytes, ... MB/s
CA -> TA: sent 10485760 bytes, ... MB/s
TA -> CA: received 10485760 bytes
```

See [vsock-TIPC throughput](docs/vsock-tipc-throughput-analysis.md) for the
path analysis and measured results.
