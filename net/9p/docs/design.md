# 9P Client Design

## Scope

The `p9` crate implements the transport-independent 9P client protocol. It
owns request and response encoding, protocol parsing, session negotiation, and
the client operations used by filesystem consumers. Its location under
`net/9p` reflects that protocol ownership; it is not itself a VFS filesystem.

## Structure

- `transport.rs` defines the provider-neutral `Transport` interface.
- `message.rs`, `parse.rs`, and `protocol.rs` implement wire-format handling.
- `session.rs` owns negotiated session state and exposes path- and fid-based
  client operations.

A concrete transport is supplied by the caller. Transport selection and device
discovery remain outside this crate.

## Concurrency and lifecycle

`P9Session` serializes protocol exchanges through mutable access. The caller
creates it with a transport and mount tag, negotiates the protocol version, and
keeps it alive for the mounted filesystem's lifetime.

This directory relocation does not change APIs, protocol behavior, state, or
execution-context assumptions.
