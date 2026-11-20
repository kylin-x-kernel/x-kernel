# Plain vsock and vsock-TIPC: message boundaries and throughput

This document describes the CA-to-TA path used by X-Kernel under QEMU, its
correspondence with the original Trusty bridge, and measured throughput. Here,
**RX** means the TA receiving from the CA, and **TX** means the TA sending to the
CA. All throughput figures use decimal **MB/s** (10^6 B/s) to match
`vsock_perf`; historical MiB/s values are converted here.

## How the two paths differ

| | Plain vsock | vsock-TIPC |
| --- | --- | --- |
| CA interface | `SOCK_STREAM` over `AF_VSOCK` | `libtrusty-rs::TipcChannel`, which uses `SOCK_STREAM` on `VSOCK:<cid>:0` |
| Guest receiver | A vsock socket read repeatedly by the application | virtio-vsock driver → bridge → TIPC message queue → TA |
| One CA `write()` | Writes to the byte stream and may span multiple virtio packets | Also writes to the byte stream; the bridge converts each RX event into one TIPC message |
| Boundary visible to the guest application | The byte count returned by `read()`; it is not guaranteed to match one CA `write()` | Each `recv_into()` returns one TIPC message; that boundary corresponds to one RX event forwarded by the bridge |

A normal 128 KiB vsock `write()` submits 128 KiB to the byte stream; it does not
create a 128 KiB virtio packet. The guest may need several `read()` calls to
consume it. `SOCK_STREAM` does not preserve application write boundaries; see
the [Linux vsock manual](https://man7.org/linux/man-pages/man7/vsock.7.html).

The CA side of vsock-TIPC also uses a byte stream, but the guest
[bridge RX path](../../../net/knet/src/vsock/bridge.rs) takes each RX event and
sends its data as one message through
[the TIPC channel](../../../net/knet/src/vsock/bridge_connection.rs). The TA
then receives one message per `recv_into()`. Consequently, one CA `write()` may
become several TA messages, and multiple CA writes are not guaranteed to remain
separate TA messages.

### An 8 KiB write with 4 KiB RX buffers

A virtio-vsock packet header consumes 44 bytes. With a 4096-byte virtqueue RX
buffer, one event can carry at most 4052 payload bytes. Linux `vhost-vsock`
splits a larger pending packet according to the guest's buffer space and
requeues the remainder; see
[Linux vhost-vsock](https://github.com/torvalds/linux/blob/v6.6/drivers/vhost/vsock.c).

For example, an 8192-byte CA write may arrive as RX events of
`4052 + 4052 + 88` bytes. The bridge then sends three TIPC messages, and the TA
performs three reads. Host packetization and flow control mean the exact split
cannot be treated as an API guarantee. The transport continues sending the
remaining bytes; the CA only has to retry bytes when its `write()` returns a
short count. The current `libtrusty-rs::TipcChannel::send()` requires one
`write()` to complete and does not yet retry a partial write.

## Does Trusty also forward one TIPC message per RX event?

Yes. In the original Trusty vsock bridge
(`trusty-c/trusty-c/external/trusty/lk/dev/virtio/vsock-rust/src/vsock.rs`),
`vsock_rx_channel()` reads each event according to its `length`, and
`tipc_try_send()` subsequently calls `ipc_send_msg()`. The original bridge also
has a TODO for large messages; it does not combine multiple RX events to
reconstruct one CA application write.

The original Trusty PCI vsock driver
(`.../lk/dev/virtio/vsock-rust/src/pci.rs`) uses a 4096-byte RX buffer. The
512-byte value in the generic driver library is only a default type parameter,
not the PCI driver's configuration. The X-Kernel RX buffer was 8 KiB before the
experiment. This branch keeps
[8 KiB](../../../drivers/devices/virtio/src/socket.rs); the release/2609 branch
used 64 KiB when `vsock-tipc` was enabled.

## Why per-message forwarding affects RX throughput

Every RX event passes through bridge event handling, one data copy, and one TIPC
send. The current [bridge](../../../net/knet/src/vsock/bridge.rs) allocates a
`Vec` for event data and stores it in the connection's staging buffer; the
[TIPC message queue](../../../tee/tipc/src/message.rs) also copies it into the
receive slot. The TA then performs `get_msg → read_msg → put_msg` and one
service callback per message. More messages therefore increase the fixed cost
per MiB.

## Historical 128 KiB configuration

That experiment set the virtio-vsock RX buffer, TIPC maximum message size, and
TA receive slot to 128 KiB in lockstep, with 128 KiB and 4052-byte CA writes as
the comparison. TA-to-CA traffic still used 4052-byte writes, so the experiment
targeted only CA-to-TA RX. The results below belong to that historical
configuration and are not a direct baseline for the current programs and
limits.

Test conditions: `make justrun ACCEL=n`, default QEMU arguments and `disk.img`,
and a total transfer of 1 MiB. The TA timer started at the first data message
and stopped after 1 MiB had been received.

| Configuration | CA write size | TA messages | TA RX |
| --- | ---: | ---: | ---: |
| Before the change | 4052 bytes | Not recorded | about 1.41 MB/s |
| Historical 128 KiB kernel, same boot | 4052 bytes | 269 | 3.49 MB/s |
| Historical 128 KiB kernel, same boot | 128 KiB | 16 | 31.30 MB/s |

Three additional 128 KiB runs measured **29.78, 29.16, and 30.74 MB/s**.
Repeated 4052-byte runs varied between **2.00 and 3.66 MB/s**; the table uses a
same-boot result for which the message count was recorded. In that comparison,
the message count fell from 269 to 16 and RX rose from 3.49 to 31.30 MB/s, about
**9x**. This strongly supports the conclusion that per-message fixed cost limits
small-message throughput. Total elapsed time alone does not apportion the cost
among the bridge, TIPC, and TA.

An independent `vsock_perf` rerun on the same class of QEMU host without KVM
measured about **0.215 Gbps (26.91 MB/s)** with the default 128 KiB buffer and
about **0.085 Gbps (10.66 MB/s)** with 4052-byte read and write buffers. In the
second case the sender still wrote full 4052-byte chunks, so it transferred
1,049,468 bytes, 892 bytes more than 1 MiB. The two programs use different timer
boundaries; plain-vsock and TA-RX values are useful only as an order-of-magnitude
reference, not as a strict end-to-end efficiency ratio.

### Limits of the historical experiment

- The 128 KiB value was the global TIPC maximum at that time; services could
  choose a smaller slot. The release/2609 branch later capped one buffer at
  64 KiB, while this branch caps it at 8 KiB. Larger slots and larger queues
  increase per-connection preallocated memory.
- A 128 KiB RX virtqueue buffer includes the 44-byte header, so one event carries
  less than 128 KiB of payload. A 128 KiB CA write may still become multiple TIPC
  messages; the experiment observed 16 messages for 1 MiB.
- The test checked cumulative byte counts but did not compare every payload byte.
- Precise attribution of the remaining cost requires recording RX-event length
  distributions, bridge processing time, `ipc_send_msg()` time, and per-message
  TA receive time.

## Current 8 KiB configuration

This branch keeps the virtio-vsock RX buffer at 8 KiB including the 44-byte
header. The TIPC maximum receive-buffer size (`IPC_CHAN_MAX_BUF_SIZE`) and the
TA receive slot are both 8 KiB, so one RX event of at most 8148 payload bytes
fits in one slot. The connection manager advertises a 256 KiB `buf_alloc`
window. The [CA](../host-companion/src/main.rs) and [TA](../src/main.rs)
transfer 10 MiB in each direction and default to 4 KiB chunks.

### plain vsock 4 KiB baseline

`vsock_perf-aarch64 --buf-size 4K` with five measured iterations on the same
QEMU TCG host reported aggregate **RX 50.88 MB/s** and **TX 42.73 MB/s**
(guest-side output). These are local baseline measurements, not a stored
benchmark artifact.

Before the bridge waited on TIPC progress, `TipcSendBlocked` used a fixed 10 ms
backoff. With 3 TIPC slots plus one staged record, that capped RX at roughly
4 x 4 KiB / 10 ms = 1.64 MB/s; one measured run reported 1.55 MB/s. The
bridge now waits on `READY`/`SEND_UNBLOCKED`/teardown wake-ups, with the timeout
used only as a liveness fallback.

Five consecutive runs with the event-driven wait, the same aarch64 QEMU TCG
setup, and 4 KiB chunks produced:

| Run | TA RX | TA TX |
| ---: | ---: | ---: |
| 1 | 15.34 MB/s | 29.79 MB/s |
| 2 | 16.21 MB/s | 29.43 MB/s |
| 3 | 15.00 MB/s | 28.67 MB/s |
| 4 | 16.00 MB/s | 29.38 MB/s |
| 5 | 15.49 MB/s | 29.34 MB/s |

RX improved by about 10x over the 10 ms backoff result. TX also measured higher
than the earlier single sample, but the RX change does not alter the
TIPC-to-vsock send path, so treat that TX difference as run-to-run variation
until it is measured with a dedicated comparison.

The 64 KiB measurements below were collected on the release/2609 branch with
the earlier 64 KiB RX buffer, TIPC limit, and TA slot. They are retained as a
historical reference and are not measurements of this branch.

## 64 KiB release configuration (historical)

With `vsock-tipc`, the release/2609 virtio-vsock RX buffer was 64 KiB including
the 44-byte header. The TIPC maximum receive-buffer size and the TA receive slot
were both 64 KiB, while the connection manager advertised a 128 KiB `buf_alloc`
window. The release [CA](../host-companion/src/main.rs) and
[TA](../src/main.rs) transfer 10 MiB in each direction and defaulted to 32 KiB
chunks.

On 2026-11-04, the reorganized uapp and host companion were built from that tree
and run in one aarch64 QEMU boot with `make justrun ACCEL=n`, the default QEMU
arguments, and the modified `disk.img`. The CA used its default 32 KiB upload
size for one run and a 4052-byte upload size for the comparison run. Both
directions transferred the full 10 MiB each time, and the host CA exited
successfully with:

```text
CA -> TA: sent 10485760 bytes
TA -> CA: received 10485760 bytes
```

The corresponding TA serial output was:

| CA → TA upload size | TA RX messages | TA RX | TA TX |
| ---: | ---: | ---: | ---: |
| 32 KiB | 320 | 29.39 MB/s | 37.95 MB/s |
| 4052 bytes | 2691 | 2.19 MB/s | 37.95 MB/s |

This rerun verifies that the colocated TA and host CA preserve the original
bidirectional transfer behavior. It is a single sample per configuration, so it
is not a statistical comparison.

Before this reorganization, on 2026-10-07, the pre-refactor programs were run
with the same QEMU mode and default 63 KiB chunks. Their TA serial output was:

| CA → TA write size | TA RX messages | TA RX | TA TX |
| ---: | ---: | ---: | ---: |
| 63 KiB | 244 | 32.73 MB/s | 44.82 MB/s |
| 4052 bytes | 2691 | 1.30 MB/s | 47.58 MB/s |

Only the CA-to-TA write size changed in these runs. TA-to-CA used a 63 KiB chunk
in both cases. The release program used 32 KiB in both directions; this branch
defaults to 4 KiB. All of these measurements describe the QEMU environment; they
do not predict throughput on Kylin 100 with crosvm and cannot be compared
directly as a performance multiple with the historical 128 KiB / 1 MiB
experiment.

See the [test README](../README.md) for build and launch steps. After starting
`/tipc-bidi-large-server` in the guest, run the host companion with the default
4 KiB upload size and then with 4052 bytes:

```sh
uapps/tipc-bidilarge-test/host-companion/target/release/tipc-bidi-large-client \
  103
uapps/tipc-bidilarge-test/host-companion/target/release/tipc-bidi-large-client \
  103 4052
```

The second command reproduces the old 4052-byte packet payload on the same
kernel. Neither command can reproduce the historical 64 KiB or 128 KiB
numbers, which depend on larger TIPC receive-buffer limits.
