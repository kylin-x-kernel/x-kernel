# host-companion — Security and Reliability

## Trust model

This is a host-side test tool. The guest and all received bytes are untrusted.
It connects only to the CID supplied on the command line and performs no
privileged host operation.

## External boundary

CLI input selects the CID and upload chunk size; the chunk is bounded to
`1..=8192` bytes. Control responses must be exactly one expected byte. The data
phase counts 10 MiB and does not verify payload contents.

## Memory safety and unsafe code

The crate contains no `unsafe`, FFI, inline assembly, or shared mutable state.
`libtrusty-rs` owns the channel and receive buffers; reads are bounded by the
caller-provided slice.

## Threats and failure modes

| Threat | Response |
| --- | --- |
| Invalid CID or chunk value | Return `InvalidInput` before connecting. |
| Malformed control response | Return `InvalidData`. |
| Early EOF or short transfer | Return `UnexpectedEof` or an `io::Error`. |
| RX throughput regression | Fail if upload throughput is below 5 MB/s. |
| Slow or unreliable guest | The transfer times out or the throughput threshold fails. |

## Limitations

The tool does not authenticate or encrypt the peer and does not verify data
integrity. The 5 MB/s threshold is a test guard for the documented QEMU TCG
environment, not a general performance requirement.
