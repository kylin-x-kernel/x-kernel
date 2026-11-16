# kernel-elf-parser — Security and Reliability Analysis

## Trust Model and External Boundaries

The ELF file header, the program header table, the load offset, and the argument and environment strings come from the caller.
File fields must be treated as untrusted input; the information returned by the parser must still be validated by the loader against the process
address space and ABI constraints, and must not be used directly as an already-validated user mapping.

## Memory-Safety Invariants and unsafe Code

The only `unsafe` code is in `src/info.rs`: each program header entry is copied out by value with `ptr::read_unaligned`.
Before the copy, `build` checks the entry size and the full table length that correspond to the ELF class, and each
entry is narrowed to a fixed-size array exactly one entry long. `xmas_elf` declares the program header types `zero::Pod`,
so every byte pattern is a valid value. The table may therefore start at any file offset: an `e_phoff` that is not a
multiple of the entry alignment is parsed, as on Linux, instead of tripping an alignment assertion.

The ELF file header is still borrowed in place by `xmas_elf::header::parse_header`, whose `zero::read` asserts alignment.
`ElfHeadersBuilder::new` checks the class-specific header part for alignment first and returns an error for a misaligned
buffer. The header sits at file offset 0, so this depends only on the caller's buffer, not on file contents; kernel heap
allocations of the file head meet it in practice, and the check turns any violation into an error.
The Rust lifetimes of the input slice and the parsed header guarantee that the borrow remains valid; the program header array is provided
to `ElfParser` through a shared reference and cannot be modified concurrently by the caller after construction.

`ElfParser::new` compares segment coverage ranges using subtraction, avoiding addition overflow when comparing the end of a range;
it then checks the additions of the virtual address, the in-table offset, and the load offset, as well as the conversions to `usize`.
A successfully stored `phdr` therefore cannot cause a panic at query time because of a missing covering segment or an address overflow.

## Fault Management / FMEA

| Condition | Handling and Impact |
| --- | --- |
| Unsupported ELF class, wrong entry size, or incomplete program header table | The builder returns an error, and the parse result is not published |
| Program header table at an offset that is not a multiple of the entry alignment | Entries are copied by value and parsed normally |
| Header buffer shorter than the ELF header, or misaligned for the class-specific header part | `ElfHeadersBuilder::new` returns an error instead of panicking |
| No segment covers the file offset of the program header table | Parser construction returns an error, instead of panicking later during auxiliary vector queries |
| Overflow when adding the program header virtual address and the load offset | Parser construction returns an error |
| Allocation failure | The original failure behavior of the container still applies; the crate adds no recovery policy |

## Concurrency, Privacy, and Known Limitations

The crate has no global registry or lock; the caller is responsible for ownership of the input and for synchronization.
The initial stack array contains the arguments, environment variables, and random bytes, and the caller is responsible for the permissions and
lifetime of the final user mapping. Ordinary container deallocation does not promise to erase this data.

The program header address check only verifies that a segment covers the starting offset of the table; it does not prove that the entire program header table
lies within a loadable segment, nor does it validate segment permissions. The address
arithmetic of other pre-existing queries such as `entry()` was not redesigned in this lint migration. The regression tests cover a missing covering segment
and program header virtual address overflow, aligned and unaligned program header tables for ELF32 and ELF64,
misaligned and truncated header buffers, as well as the successful construction path for a valid ELF.
