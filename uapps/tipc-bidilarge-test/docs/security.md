# tipc-bidilarge-test — Security and Reliability

## Trust model

This is a test-only TA. The host CA and all bytes arriving through the vsock-TIPC
bridge are untrusted. The TA holds no secrets and persists no state outside the
process lifetime.

## External boundary

The only input is a sequence of TIPC messages, each bounded by the 8 KiB receive
slot. Control messages are one byte and are matched exactly. Data payloads are
counted but not verified; malformed control messages or an oversized transfer
close the connection.

## Memory safety and unsafe code

The crate contains no `unsafe`, FFI, inline assembly, or custom synchronization.
Message buffers are owned by `rust_libtipc`'s `Manager`; `recv_into` bounds each
read by the supplied buffer. Dependency safety is outside this crate's audit.

## Threats and failure modes

| Threat | Response |
| --- | --- |
| Malformed or out-of-order control message | Close the connection and print the unexpected length. |
| Payload larger than the TIPC slot | TIPC or the bridge rejects it; the connection does not reach `Complete`. |
| Slow or stalled host | Timing reports the lower rate; the test has no data-integrity guarantee. |
| Port configuration above the kernel limit | Startup returns an error instead of panicking. |

## Limitations

The test does not authenticate the peer, encrypt traffic, verify payload
contents, or provide production isolation. A successful run only demonstrates
that the configured transfer completed and reports the measured rate.
