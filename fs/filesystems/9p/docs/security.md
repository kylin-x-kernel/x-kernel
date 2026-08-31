# 9P Filesystem Security

## Trust boundary

The filesystem translates local VFS requests to operations against an
untrusted remote 9P service. File metadata, directory entries, link targets,
and file contents returned by that service must be treated as external input.

## Invariants and failure handling

- VFS operation arguments are translated at the filesystem boundary before
  protocol requests are issued.
- Protocol failures are converted to VFS-visible errors at the filesystem
  boundary.
- Shared session access remains serialized by the filesystem's lock.
- Object lifetime must not outlive the mounted filesystem and its session.

The crate contains no `unsafe` blocks. Wire-format validation belongs to the
`p9` client, while transport memory and device safety belong to the concrete
transport provider.

## Current limitations

The current protocol client uses string errors, so the filesystem maps those
failures through its existing conversion layer. Improving typed error fidelity
or changing permission semantics is outside this source layout change.
