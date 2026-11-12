# kernel-image-metadata — Design Document

## Positioning

`kernel-image-metadata` owns the X-Kernel image metadata ABI. XKMake uses it to encode
ELF notes, and the running kernel uses it to locate, validate, and read the same metadata.

## Background

Build provenance information is written into the bundle ELF after Cargo linking completes, so that diagnostic
fields such as the machine name and build time do not trigger a recompilation of the kernel crate. This data belongs to both the host packager and the kernel
image, so it lives in a shared ABI crate under `boot/` rather than in the general-purpose `util` crate.

## Scope

- `src/lib.rs`: section names, ELF note constants, encoding, validation, and kernel linker
  symbol access.
- `docs/design.md`: the image ABI, calling constraints, and design decisions.
- `docs/security.md`: the trust boundary between ELF bytes and linker ranges.

## Architecture

```text
ResolvedKernelConfig
        |
        v
      XKMake ---- encode/finalize ----> kernel ELF notes
                                           |
                                           v
                              kernel-image-metadata
                                           |
                                           v
                                     early boot log
```

`.note.xkernel.build-info` uses the standard ELF note wrapper, and its fixed-size descriptor
contains a versioned header and a UTF-8 payload. `.note.gnu.build-id` uses a SHA-256 descriptor.
Both descriptors are reserved by the linker and placed in a load segment, and XKMake only writes to them in place.
After finalization, the section header exposes only the actual encoded length, but the fixed
reserve delimited by the linker symbols is still fully retained in the same `PT_LOAD`,
so that the early kernel can perform bounded parsing.

## Calling Constraints / Execution Context

The host-side encoding API operates only on slices provided by the caller. With the `kernel` feature enabled, the read API
can be called before the allocator, scheduler, and interrupt system are initialized; it does not allocate, does not block, and does not depend on
the current process. It relies on the four boundary symbols provided by the final linker script, and the corresponding memory must
already be mapped together with the kernel's read-only segments.

## Algorithm Flow

1. The linker reserves two allocatable ELF note descriptors in the final address layout.
2. XKMake validates the section, the note header, and the load segment in the temporary bundle ELF.
3. The build-info descriptor is zeroed, and the little-endian header, CRC32, and UTF-8 payload are written.
4. After the Build ID descriptor is zeroed, SHA-256 is computed over the final `PT_LOAD` bytes and written back.
5. The kernel obtains the descriptor slices through the linker symbols, validates them, and then reads them.

## Concurrency Model

The crate holds no mutable global state. Host-side encoding requires an exclusive slice; kernel-side reading only borrows the final
read-only image, and the API is reentrant and needs no synchronization.

## Design Decisions

- ELF notes are used to express image metadata, and the section name and owner distinguish X-Kernel from GNU.
- A fixed descriptor size keeps the program segment layout stable, avoiding a mismatch between
  `PT_LOAD` and the raw image caused by adding sections after linking.
- The payload is text; the kernel only needs to validate and print it, while the semantic field model remains owned by XKMake.
- The Build ID covers the final loadable image, rather than non-runtime section header noise.

## Drop / Resource Release

The crate owns no resources. All return values borrow the caller's slice or the static kernel image.
