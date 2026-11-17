# kernel-elf-parser — Design Document

## Positioning and Scope

This crate provides ELF header parsing, the auxiliary vector representation, and the byte layout of the initial user stack.
It does not own the process address space, and it is not responsible for mapping or loading ELF segments.
`src/info.rs` manages the parse results, `src/auxv.rs` represents the auxiliary vector,
and `src/user_stack.rs` generates the stack contents; `src/error.rs` defines the `ElfError`
shared by parsing and stack construction, keeping each failure condition a distinct variant. See the rustdoc for the parameters and error contracts of the public API.

## Architecture and Algorithm Flow

`ElfHeadersBuilder` borrows the ELF file header, checks the program header entry size and the table range,
and then copies the full program header table passed in into a uniform `ProgramHeader64` array.
`ElfParser` borrows these headers and chooses the load offset according to the ELF type.
During construction, it looks for the segment that covers the file offset of the program header table, and computes
the virtual address of the program header table using checked addition and integer conversion. A missing covering segment and an address overflow return
`ElfError::ProgramHeaderTableUnmapped` and `ElfError::ProgramHeaderAddressOverflow` respectively;
the diagnostic text matches the former string errors.

After successful construction, the `phdr` field retains this address; `phdr()` and `aux_vector()`
no longer search the headers again and no longer panic because of a missing covering segment. The headers are borrowed through a shared reference,
so the caller cannot modify the array used for validation during the lifetime of the parser.
`app_stack_region` independently receives the auxiliary vector and strings, and generates a byte array that the loader can copy into
user memory; it does not access the user address space directly.

## Calling Constraints / Execution Context

Parsing the program header table and generating the user stack require `alloc`, so the caller should choose a context in which allocation is allowed.
These APIs do not depend on the current process, CPU-local state, the scheduler, or device initialization,
and they do not sleep, perform I/O, or modify hardware page tables on their own. They have no global locks or mutable global state.

## Design Decisions and Resource Release

The program header address is validated and saved during the fallible construction phase, so that subsequent auxiliary vector queries keep
their original error-free return interface. The validation only fulfills the address contract required by that query; it cannot replace
the loader's checks of segment types, mappable ranges, permissions, or the mapping of the entire program header table.
The parsed array and the stack bytes are released by ordinary containers; the parser itself only borrows the headers and does not own any mappings.
