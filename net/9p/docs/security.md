# 9P Client Security

## Trust boundary

The transport peer and every received 9P response are untrusted. Parsing must
validate message sizes, field boundaries, and string encodings before exposing
data to callers. Paths and operation parameters supplied by callers also cross
the protocol boundary.

## Invariants and failure handling

- Protocol parsing must not read beyond the received message.
- A response must match the expected operation before its payload is consumed.
- Transport and protocol failures are returned to the caller as errors.
- Session and fid lifecycle operations must remain ordered through the mutable
  session interface.

The crate contains no `unsafe` blocks. Concrete transport safety, DMA ownership,
and device lifetime are responsibilities of the transport provider.

## Current limitations

The public client interface currently reports protocol failures as strings.
Changing error types or protocol semantics is outside the scope of this source
layout change.
