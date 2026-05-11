---
name: asterinas-coding-guidelines
description: Use when writing or reviewing Rust kernel code in the x-kernel project — covers naming, comments, layout, formatting, API design, Rust-specific rules, unsafe code, concurrency, error handling, logging, memory management, performance, git commits, testing, and assembly conventions
---

# Coding Guidelines for x-kernel

Based on: https://github.com/asterinas/asterinas/tree/main/book/src/to-contribute/coding-guidelines

These guidelines aim to keep code clear, consistent, maintainable, correct, and efficient.

---

# General Guidelines

## Names

### Be descriptive (`descriptive-names`) {#descriptive-names}

Choose names that convey meaning at the point of use.
Avoid single-letter names and ambiguous abbreviations.
Prefer full words over cryptic shorthand
so that readers do not need surrounding context
to understand a variable's purpose.
Prefer names that are as short as possible
while still being unambiguous at the point of use.

### Be accurate (`accurate-names`) {#accurate-names}

Avoid confusing names.
If a name can be misread
to imply the wrong meaning, behavior, or side effects,
it must be corrected immediately.

```rust
// Good — clearly a count
nr_deleted_watches: usize,
// Bad — looks like a collection
// rather than a numeric counter
deleted_watches: usize
```

Choose verbs that reflect the actual work being done.

```rust
impl PciCommonDevice {
    // Good — implies a MMIO read is involved
    pub fn read_command(&self) -> Command { /* .. */ }
    // Bad — looks like a plain field access
    pub fn command(&self) -> Command { /* .. */ }
}
```

```rust
mod char_device {
    // Good — implies an O(n) collection pass
    pub fn collect_all() -> Vec<Arc<dyn Device>> { /* .. */ }
    // Bad — sounds like an accessor returning a reference
    pub fn get_all() -> Vec<Arc<dyn Device>> { /* .. */ }
}
```

### Encode units and important attributes in names (`encode-units`) {#encode-units}

When the type does not encode the unit,
the name must.
Kernel code deals with bytes, pages, frames,
nanoseconds, ticks, and sectors —
ambiguous units are a source of real bugs.

```text
// Good — unit is unambiguous
timeout_ns
offset_bytes
size_pages
delay_ms

// Bad — unit is ambiguous
timeout
offset
size
delay
```

Where the language's type system can enforce units (e.g., newtypes),
prefer that.
Where it cannot, the name must carry the information.

### Use assertion-style boolean names (`bool-names`) {#bool-names}

Boolean variables and functions
should read as assertions of fact.
Use `is_`, `has_`, `can_`, `should_`, `was_`,
or `needs_` prefixes.
Never use negated names
(`is_not_empty`, `no_error`);
prefer the positive form
(`is_empty`, `ok` or `succeeded`).
A bare name like `found`, `done`, or `ready`
is acceptable when the context is unambiguous.

```rust
// Good — reads as an assertion
fn is_page_aligned(&self) -> bool { ... }
fn has_permission(&self, perm: Permission) -> bool { ... }
let can_read = mode.is_readable();

// Bad — verb suggests an action, not a query
fn check_permission(&self, perm: Permission) -> bool { ... }
// Bad — negated name
let is_not_empty = !buf.is_empty();
```

## Comments

### Prefer semantic line breaks (`semantic-line-breaks`) {#semantic-line-breaks}

For prose in Markdown and doc comments,
insert line breaks at semantic boundaries
so each line carries one coherent idea.
At minimum, break at sentence boundaries.
For longer sentences, also consider breaking at clause boundaries.

Semantic line breaks make diffs smaller,
reviews easier,
and merge conflicts less noisy.

As an exception,
RFC documents that are mostly read-only
can use regular paragraph wrapping.

### Explain why, not what (`explain-why`) {#explain-why}

Comments should explain the intent behind the code,
not restate what the code does.
If a comment merely paraphrases the code,
it adds noise without insight.

If a comment is needed to explain what code does,
first try to rewrite the code.
Do not write good comments to compensate for bad code —
rewrite it to be straightforward.

### Document design decisions (`design-decisions`) {#design-decisions}

When the code makes a non-obvious choice —
a particular data structure, a locking strategy,
a deviation from Linux behavior —
add a comment explaining the rationale
and any alternatives considered.
Design-decision comments ("director's commentary")
are the most valuable kind of comment.

```rust
// We use a radix tree rather than a HashMap
// because lookups must be O(log n) worst-case
// for the page fault handler.
// A HashMap gives O(1) amortized
// but O(n) worst-case due to rehashing,
// which is unacceptable on the page fault path.
```

### Cite specifications and algorithm sources (`cite-sources`) {#cite-sources}

When implementing behavior defined by
an external specification or a non-trivial algorithm,
cite the source:
the relevant POSIX section, Linux man page,
hardware reference manual, or academic paper.

```rust
/// Maximum number of bytes guaranteed to be written to a pipe atomically.
///
/// For more details, see the description of `PIPE_BUF` in
/// <https://man7.org/linux/man-pages/man7/pipe.7.html>.
const PIPE_BUF: usize = 4096;
```

## Layout

### One concept per file (`one-concept-per-file`) {#one-concept-per-file}

When a file grows long or contains multiple distinct concepts,
split it.
Each major data structure, each subsystem entry point,
each significant abstraction
deserves its own file.

### Organize code for top-down reading (`top-down-reading`) {#top-down-reading}

A source file should read from top to bottom.
Start with high-level entry points and core flow.
Move implementation details downward
so readers can understand the big picture first
before diving into low-level helpers.

Within each visibility group (e.g., a module),
order methods so that callers appear before callees where possible,
enabling the file to be read top to bottom.
Place public methods before private helpers.

### Group statements into logical paragraphs (`logical-paragraphs`) {#logical-paragraphs}

Within functions,
group related statements into logical paragraphs
separated by blank lines.
Each paragraph should represent one sub-step
of the function's overall purpose.

For long functions,
add a one-line summary comment
at the start of each paragraph
when the paragraph intent is not obvious.

## Formatting

### Format error messages consistently (`error-message-format`) {#error-message-format}

Start with a lowercase letter
(unless the first word is a proper noun or identifier).
Be specific:
prefer "`len` is too large" over "the argument is invalid".

For system call errors,
follow the style and descriptions in Linux man pages.

## API Design

### Stick to familiar conventions (`familiar-conventions`) {#familiar-conventions}

Prefer names and API shapes
that users already know from Rust and Linux.
Do not invent new terms
for well-known operations.

```rust
// Good — follows common Rust naming conventions
pub fn len(&self) -> usize { ... }
pub fn as_ptr(&self) -> *const u8 { ... }

// Bad — unfamiliar synonyms for common operations
pub fn length(&self) -> usize { ... }
pub fn to_pointer(&self) -> *const u8 { ... }
```

### Hide implementation details (`hide-impl-details`) {#hide-impl-details}

Do not expose internal implementation details
through public APIs (including their documentation).
A module's public surface
should contain only what its consumers need.

### Encode current-context semantics in APIs (`current-context-semantics`) {#current-context-semantics}

When code needs a "current" resource,
the API should state whether it requires
a current **process thread**
or only a current **execution path**.
Do not force callers to infer this from hidden assumptions.

For filesystem context access in x-kernel:

- Use `kthread::current_process_fs_context()`
  for process-only paths
  such as syscalls and POSIX helpers.
- Use `kthread::current_fs_context()`
  for shared helpers
  that may run from either a user thread
  or a kernel task.
- Prefer passing `&FsContext` or `Arc<Mutex<FsContext>>`
  into deeper helpers
  rather than reading current context implicitly.

### Use the correct fd abstraction layer (`fd-abstraction-layers`) {#fd-abstraction-layers}

File descriptor operations use a three-layer architecture.
Each layer has a clear responsibility.
Callers must use the highest layer
that satisfies their need.

| Layer | Crate | Type | Responsibility |
|-------|-------|------|----------------|
| Low-level | `kfd` | `FdTable` | Table storage, insert/remove, no policy |
| Process | `kresources` | `ProcessResources` | Per-process wrappers with rlimit enforcement |
| Current-context | `kthread` | `current_resources()` | Shortcut for the current process |

#### Low-level: `kfd::FdTable`

`FdTable` owns the descriptor storage.
Its methods are thin table operations:
`get`, `add`, `remove`, `add_at`, `clone_from`.
They take `&mut self` and enforce no resource-limit policy.

Use this layer only when building new process-level wrappers
or when you already hold a direct `&mut FdTable` reference.

#### Process: `kresources::ProcessResources`

`ProcessResources` wraps `Arc<RwLock<FdTable>>`
and exposes named methods that enforce process policy
(rlimit checks, close-on-exec semantics, unsharing).

```rust
// Good — uses the process layer
let resources = current_resources();
let file = resources.get_file_like(fd)?;
let new_fd = resources.add_file_like(file, cloexec)?;
resources.close_file_like(fd)?;
```

```rust
// Bad — reaches into the fd_table Arc directly
// from code that only cares about the current process
let fd_table = current_process_state().resources.fd_table();
let file = fd_table.read().get_file_like(fd)?;
```

Internal helpers use `with_fd_table`
to avoid cloning the `Arc` for single-access operations:

```rust
fn with_fd_table<R>(&self, access_fn: impl FnOnce(&RwLock<FdTable>) -> R) -> R {
    let fd_table = self.fd_table.read();
    access_fn((*fd_table).as_ref())
}
```

#### Current-context: `kthread::current_resources()`

For code that operates on the *current* process
(typical in syscall handlers),
use `kthread::current_resources()`.
It returns `Arc<ProcessResources>`
and avoids the verbose
`current_process_state().resources.clone()` spell.

```rust
// Good — concise, uses the current-context shortcut
use kthread::current_resources;
let file = current_resources().get_file_like(fd)?;
```

#### Type-specific lookups: `get_file_like_as`

`ProcessResources::get_file_like_as::<T>` delegates
to `T::from_fd(fd_table, fd)` rather than
a generic `downcast`.
This preserves type-specific error codes
(e.g., `NotASocket` instead of a generic `InvalidInput`).

```rust
// Good — returns NotASocket on mismatch
let socket = current_resources().get_file_like_as::<Socket>(fd)?;

// Bad — returns InvalidInput on mismatch
let file = current_resources().get_file_like(fd)?;
let socket = file.downcast_arc::<Socket>().map_err(|_| KError::InvalidInput)?;
```

#### Encapsulation: `FileDescriptor` fields are private

`FileDescriptor` in `kfd` exposes its fields through getters
(`inner()`, `cloexec()`, `set_cloexec()`).
Direct field access is not permitted outside `kfd`.

### Validate at boundaries, trust internally (`validate-at-boundaries`) {#validate-at-boundaries}

Designate certain interfaces as validation boundaries.
In Asterinas, syscall entry points
are the primary boundary:
all user-supplied data
(pointers, file descriptors, sizes, flags, strings)
must be validated at the syscall boundary.
Once validated, internal kernel functions
may trust these values without re-validation.

### Copy user memory at syscall boundaries (`syscall-user-copy`) {#syscall-user-copy}

POSIX syscall frontends must treat user pointers
as copy-in / copy-out handles,
not borrowed references into user memory.

Use [`posix_types::UserConstPtr`] and [`posix_types::UserPtr`]
at the syscall boundary.
Copy user input into kernel-owned values immediately,
and copy results back to user memory only at the end
of the syscall path.
Do not pass user pointers deeper into subsystem logic
once a kernel value can be formed.

For typed copy-in,
prefer `read_vm()`.
For typed copy-out,
prefer `write_vm()` / `write_vm_slice()`.
For raw byte buffers and strings,
use the byte/string helpers instead of inventing ad-hoc pointer access.

Do not open-code syscall copies with
`read_uninit()?.assume_init()`,
local `read_user_*` helpers,
or borrowed user-memory slice tricks.
If a type cannot safely participate in the generic typed helpers
because of padding or Rust representation issues,
introduce an explicit raw ABI wrapper
and convert that wrapper at the syscall boundary.

This keeps Linux-style copy semantics consistent:
unaligned user addresses are accepted for byte-copy helpers,
fault handling stays inside the shared user-copy path,
and subsystem code only sees validated kernel-owned data.

### Separate ABI carriers from kernel semantic types (`abi-carrier-separation`) {#abi-carrier-separation}

If a type appears directly in
[`posix_types::UserConstPtr<T>`] or [`posix_types::UserPtr<T>`],
it is part of the user-visible ABI boundary,
not a purely internal semantic type.

For POSIX/Linux-facing code in x-kernel:

- Put raw ABI carrier types in `posix-types`.
  This includes both standardized external layouts
  and project-defined raw wrappers
  such as `k_sigaction`, `k_sigset`, `k_siginfo`, `k_sigaltstack`,
  `msqid_ds`, `shmid_ds`, and `msgbuf`.
- Keep subsystem semantic types in the owning subsystem crate.
  Examples:
  `SignalSet`, `SignalInfo`, `SignalStack`, and `SignalAction`
  belong to `ksignal` as in-kernel semantic types.
- Convert between raw ABI carriers and semantic types
  immediately at the syscall boundary.
  Do not pass raw carriers deeper into subsystem logic
  once a semantic kernel value can be formed.
- Do not implement `UserRead` / `UserWrite`
  on semantic kernel types
  merely to make syscall code shorter.
  If user-copy is needed,
  introduce or reuse an explicit raw ABI carrier instead.

This rule keeps representation concerns,
padding/layout constraints,
and Linux ABI compatibility
out of subsystem semantic types.

### Keep nullable-pointer semantics explicit (`nullable-user-pointers`) {#nullable-user-pointers}

`UserConstPtr` and `UserPtr`
describe how to copy user memory.
They do not by themselves encode whether `NULL`
is a valid syscall argument.

If a syscall argument is optional,
check that explicitly at the syscall boundary
with `check_non_null()` or the local equivalent.
If a syscall argument is required,
read or write it directly
and let the normal fault/error path apply.

Do not hide optional-pointer semantics
inside generic user-copy helpers.
Whether `NULL` is legal
is part of the syscall ABI contract,
not part of the pointer-copy mechanism.

### Preserve Linux syscall argument shapes (`linux-abi-argument-shapes`) {#linux-abi-argument-shapes}

Keep Linux syscall argument shapes intact
unless a wrapper carries a real invariant
or materially improves correctness.

For example,
`ioctl(fd, cmd, arg)` should keep `arg: usize`
at the syscall boundary,
because the third argument is a raw ABI slot
whose meaning depends on `cmd`.
Interpret that slot in each command branch
with `UserConstPtr<T>` / `UserPtr<T>` or scalar decoding as needed,
rather than introducing a thin wrapper
that merely renames the raw integer.

---

# Rust Guidelines

Asterinas follows the
[Rust API Guidelines](https://rust-lang.github.io/api-guidelines/)
and the project-specific conventions below.

## Naming

### Follow Rust CamelCase and acronym capitalization (`camel-case-acronyms`) {#camel-case-acronyms}

Type names follow Rust's CamelCase convention.
Acronyms are title-cased per the Rust API Guidelines:

```rust
// Good
IoMemoryArea
PciDeviceLocation
Nvme
Tcp

// Bad
IOMemoryArea
PCIDeviceLocation
NVMe
TCP
```

### End closure variables with `_fn` (`closure-fn-suffix`) {#closure-fn-suffix}

Variables holding closures or function pointers
must signal they are callable by ending with `_fn`.
Treating a closure variable
as if it were a data object misleads readers.

```rust
// Good — clearly a callable
let task_fn = self.func.take().unwrap();
let thread_fn = move || {
    let _ = oops::catch_panics_as_oops(task_fn);
    current_thread!().exit();
};

let expired_fn = move |_guard: TimerGuard| {
    ticks.fetch_add(1, Ordering::Relaxed);
    pollee.notify(IoEvents::IN);
};
```

## Language Items

### Variables, Expressions, and Statements

#### Introduce explaining variables (`explain-variables`) {#explain-variables}

Break down complex expressions
by assigning intermediate results to well-named variables.
An explaining variable turns an opaque expression
into self-documenting code:

```rust
// Good — intent is clear
let is_page_aligned = addr % PAGE_SIZE == 0;
let is_within_range = addr < max_addr;
debug_assert!(is_page_aligned && is_within_range);

// Bad — reader must parse the whole expression
debug_assert!(addr % PAGE_SIZE == 0 && addr < max_addr);
```

#### Use block expressions to scope temporary state (`block-expressions`) {#block-expressions}

Use block expressions
when temporary variables are only needed
to produce one final value.
This keeps temporary state local
and avoids leaking one-off names into outer scope.

```rust
// Good — intermediate values are scoped to the block
let socket_addr = {
    let bytes = read_bytes_from_user(addr, len as usize)?;
    parse_socket_addr(&bytes)?
};
connect(socket_addr)?;

// Bad — temporary variables leak into outer scope
let bytes = read_bytes_from_user(addr, len as usize)?;
let socket_addr = parse_socket_addr(&bytes)?;
connect(socket_addr)?;
```

#### Use checked or saturating arithmetic (`checked-arithmetic`) {#checked-arithmetic}

Use checked or saturating arithmetic
for operations that could overflow.
Prefer explicit overflow handling
over silent wrapping:

```rust
// Good — overflow is handled explicitly
let total = base.checked_add(offset)
    .ok_or(Error::new(Errno::EOVERFLOW))?;

// Good — clamps instead of wrapping
let remaining = budget.saturating_sub(cost);

// Bad — may silently wrap in release builds
let total = base + offset;
```

If wraparound behavior is intentional,
use explicit `wrapping_*` or `overflowing_*` operations
and document why wrapping is correct.

### Functions and Methods

#### Minimize nesting (`minimize-nesting`) {#minimize-nesting}

Minimize nesting depth.
Code nested more than three levels deep
should be reviewed for refactoring opportunities.
Each nesting level multiplies the reader's cognitive load.

Techniques for flattening nesting:
- Early returns and guard clauses for error paths.
- `let...else` to collapse `if let` chains.
- The `?` operator for error propagation.
- `continue` to skip loop iterations.
- Extracting the nested body into a helper function.

The normal/expected code path
should be the first visible path;
error and edge cases
should be handled and dismissed early.

```rust
pub(crate) fn init() {
    let Some(framebuffer_arg) = boot_info().framebuffer_arg else {
        warn!("Framebuffer not found");
        return;
    };
    // ... main logic at the top level
}
```

#### Keep functions small and focused (`small-functions`) {#small-functions}

Each function should do one thing,
do it well, and do it only.
If you can extract another function from it
with a name that is not merely a restatement
of its implementation,
the original function is doing more than one thing.

Do not mix levels of abstraction.
For example, a syscall handler should read like a specification;
byte-level manipulation belongs in a helper.

```rust
// Good — each function operates at one level of abstraction
pub fn sys_connect(sockfd: i32, addr: Vaddr, len: u32) -> Result<()> {
    let socket = get_socket(sockfd)?;
    let remote_addr = parse_socket_addr(addr, len)?;
    socket.connect(remote_addr)
}

// Bad — mixes high-level logic with low-level details
pub fn sys_connect(sockfd: i32, addr: Vaddr, len: u32) -> Result<()> {
    let fd_table = current_process().fd_table().lock();
    let file = fd_table.get(sockfd).ok_or(Errno::EBADF)?;
    let socket = file.downcast_ref::<Socket>().ok_or(Errno::ENOTSOCK)?;
    let bytes = read_bytes_from_user(addr, len as usize)?;
    let family = u16::from_ne_bytes([bytes[0], bytes[1]]);
    // ... 30 more lines of byte parsing ...
}
```

#### Avoid boolean arguments (`no-bool-args`) {#no-bool-args}

A boolean parameter that selects between
two behaviors signals the function does two things.
Split it into two functions
or use a typed enum.

```rust
// Good — two separate functions
fn read(&self, buf: &mut [u8]) -> Result<usize> { ... }
fn read_nonblocking(&self, buf: &mut [u8]) -> Result<usize> { ... }

// Good — typed enum
enum ReadMode { Blocking, NonBlocking }
fn read(&self, buf: &mut [u8], mode: ReadMode) -> Result<usize> { ... }

// Bad — boolean argument
fn read(&self, buf: &mut [u8], blocking: bool) -> Result<usize> { ... }
```

### Types and Traits

#### Use types to enforce invariants (`rust-type-invariants`) {#rust-type-invariants}

Leverage the type system
to make illegal states _unrepresentable_.

Define newtypes to encode domain constraints.

```rust
// Good — a `Nice` value is guaranteed to be valid
pub struct Nice(NiceValue);
type NiceValue = RangedI8<-20, 19>;

// Bad — `i8` admits invalid values for nice levels
pub type Nice = i8;
```

Prefer enums over bare integers and boolean flags.

```rust
// Good — access mode is constrained by the enum
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AccessMode {
    O_RDONLY = 0,
    O_WRONLY = 1,
    O_RDWR = 2,
}

// Bad — `u8` admits invalid values
pub type AccessMode = u8;
```

Encode invariants in generic parameters where needed.

```rust
impl IoMem<Sensitive> {
    // Good — only unsafe code can write to sensitive MMIO
    pub unsafe fn write_u32(&self, offset: usize, new_val: u32) { /* .. */ }
}

impl IoMem<Insensitive> {
    // Good — safe code can write to insensitive MMIO
    pub fn write_u32(&self, offset: usize, new_val: u32) { /* .. */ }
}

pub enum Sensitive {}
pub enum Insensitive {}
```

#### Prefer enum over trait objects for closed sets (`enum-over-dyn`) {#enum-over-dyn}

When the set of variants is known and closed,
an enum is often preferable to `Box<dyn Trait>`
for both performance and pattern-matching expressiveness.

```rust
// Good — closed set modeled as an enum
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TermStatus {
    Exited(u8),
    Killed(SigNum),
}
```

#### Encapsulate fields behind getters (`getter-encapsulation`) {#getter-encapsulation}

Do not make fields public
when a simple getter method would do.
A getter preserves naming flexibility
and leaves room for future invariants.

```rust
// Good — field is private, accessed via getter
pub struct Vma {
    perms: VmPerms,
}

impl Vma {
    pub fn perms(&self) -> VmPerms {
        self.perms
    }
}

// Bad — public field exposes representation
pub struct Vma {
    pub perms: VmPerms,
}
```

### Comments and Documentation

#### Follow RFC 1574 summary line conventions (`rfc1574-summary`) {#rfc1574-summary}

The first line of a doc comment should be concise and one sentence.
Its grammatical form depends on what the item is:

- **Functions and methods** — third-person singular present indicative verb
  ("Returns", "Creates", "Acquires"), describing the action performed.
- **Types (structs, enums, traits, type aliases), modules, and fields** —
  a noun phrase naming the thing, not describing an action.
  This matches the Rust standard library convention
  (e.g., `Vec` is "A contiguous growable array type").

```rust
/// Returns the mapping's start address.
pub fn map_to_addr(&self) -> Vaddr {
    self.map_to_addr
}

/// A policy for how [`FsPath::from_fd_at`] treats an empty `path_str`.
pub enum EmptyPathStr { /* ... */ }

/// A guard that releases a [`SpinLock`] when dropped.
pub struct SpinLockGuard<'a, T> { /* ... */ }
```

#### End sentence comments with punctuation (`comment-punctuation`) {#comment-punctuation}

If a comment line is a full sentence,
end it with proper punctuation.
This improves readability in dense code
and avoids fragmented prose.

```rust
// Good — complete sentence with punctuation.
// SAFETY: The pointer is derived from a live allocation.

// Bad — complete sentence without punctuation
// SAFETY: The pointer is derived from a live allocation
```

#### Wrap identifiers in backticks (`backtick-identifiers`) {#backtick-identifiers}

Type names, method names,
and code identifiers in doc comments
should be wrapped in backticks for rustdoc rendering.
When referring to types,
prefer rustdoc links (`[TypeName]`) where possible.

```rust
/// Acquires the [`SpinLock`] and returns a guard
/// that releases the lock on [`Drop`].
///
/// Callers must not call `acquire` while holding
/// a [`RwMutex`] to avoid deadlock.
pub fn acquire(&self) -> SpinLockGuard<'_, T> { ... }
```

#### Do not disclose implementation details in doc comments (`no-impl-in-docs`) {#no-impl-in-docs}

Doc comments should describe _what_ the API does
and _how to use it_,
not _how it is implemented internally_.

```rust
// Good — behavior-oriented
/// Returns the number of active connections.

// Bad — leaks implementation details
/// Returns the length of the internal `HashMap`
/// that tracks connections by socket address.
```

#### Add module-level documentation for major components (`module-docs`) {#module-docs}

A module file that serves as
an important kernel component
(e.g., subsystem entry point, major data structure, driver)
should begin with a `//!` comment explaining:
1. What the module does
2. The key types it exposes
3. How it relates to neighboring modules

```rust
//! Virtual memory area (VMA) management.
//!
//! This module defines [`VmMapping`] and associated types,
//! which represent contiguous regions of a process's virtual address space.
//! VMAs are managed by the [`Vmar`] tree in the parent module.
```

### Unsafety

#### Justify every use of `unsafe` (`justify-unsafe-use`) {#justify-unsafe-use}

Every `unsafe` block must have a preceding `// SAFETY:` comment
that justifies why the operation is sound.
For multi-condition invariants,
use a numbered list:

```rust
// SAFETY:
// 1. We have exclusive access to both the current context
//    and the next context (see above).
// 2. The next context is valid (because it is either
//    correctly initialized or written by a previous
//    `context_switch`).
unsafe {
    context_switch(next_task_ctx_ptr, current_task_ctx_ptr);
}
```

#### Document safety conditions (`document-safety-conds`) {#document-safety-conds}

All `unsafe` functions and traits
must include a `# Safety` section in their doc comments
describing the conditions, properties, or invariants that callers must uphold.
State exactly what the caller must guarantee —
not implementation details or side effects.

```rust
/// A marker trait for guard types that enforce the atomic mode.
///
/// # Safety
///
/// The implementer must ensure that the atomic mode is maintained while
/// the guard type is alive.
pub unsafe trait InAtomicMode: core::fmt::Debug {}
```

#### Deny unsafe code in `kernel/` (`deny-unsafe-kernel`) {#deny-unsafe-kernel}

All crates under `kernel/` must deny unsafe:

```rust
#![deny(unsafe_code)]
```

Only OSTD (`ostd/`) crates may contain `unsafe` code.
If a kernel crate requires an unsafe operation,
the functionality should be provided as a safe API in OSTD.

#### Reason about safety at the module boundary (`module-boundary-safety`) {#module-boundary-safety}

The safety of an `unsafe` block
depends on ALL code that can access the same private state.
Encapsulate unsafe abstractions
in the smallest possible module
to minimize the "audit surface."
Any code in the same module
that can modify relied-upon fields
is part of the safety argument.

```rust
// Good — small, focused module limits the audit surface
mod frame_allocator {
    /// Invariant: `next` is always a valid frame index.
    struct FrameAlloc {
        next: usize,
        // ...
    }

    impl FrameAlloc {
        pub fn alloc(&mut self) -> PhysAddr {
            // SAFETY: `next` is always valid (see invariant above).
            // Only code in this module can modify `next`.
            unsafe { self.alloc_frame_unchecked(self.next) }
        }
    }
}
```

### Modules and Crates

#### Default to the narrowest visibility (`narrow-visibility`) {#narrow-visibility}

Start private,
then widen to `pub(super)`, `pub(crate)`, or `pub`
only when an actual external consumer requires it.

```rust
// Good — restricted to the parent module
pub(super) static I8042_CONTROLLER:
    Once<SpinLock<I8042Controller, LocalIrqDisabled>> = Once::new();

pub(super) fn init() -> Result<(), I8042ControllerError> {
    // ...
}

// Bad — unnecessarily wide
pub static I8042_CONTROLLER: ...
```

Inside the `aster-kernel` crate, `pub(crate)` and `pub` are equivalent,
as the crate has no downstream consumers.
Prefer the shorter `pub`.

#### Qualify function calls with the parent module (`qualified-fn-imports`) {#qualified-fn-imports}

When importing a free function or a static/constant
from another module,
import the **parent module** and access the item
through it (`module::function()`, `module::CONSTANT`).
Do not import free functions or statics directly by name.

This convention is recommended by
[*The Rust Programming Language*](https://doc.rust-lang.org/book/ch07-04-bringing-paths-into-scope-with-the-use-keyword.html)
and followed by the Rust compiler codebase.
It serves two purposes:

1. The call site makes it clear
   that an imported item is being used,
   not a local one.
2. The module name provides context
   that complements the item name.

```rust
// Good — module-qualified function call
use ostd::irq;

let guard = irq::disable_local();

// Good — module-qualified static access
use ostd::mm::kspace;

let base = kspace::LINEAR_MAPPING_BASE_VADDR;

// Bad — bare function name; unclear origin at call site
use ostd::irq::disable_local;

let guard = disable_local();

// Bad — bare static name; could be mistaken for a local constant
use ostd::mm::kspace::LINEAR_MAPPING_BASE_VADDR;

let base = LINEAR_MAPPING_BASE_VADDR;
```

This rule applies to **free functions and statics/constants**.
Types, traits, and enum variants
should still be imported directly by name,
following the standard Rust convention.

#### Use workspace dependencies (`workspace-deps`) {#workspace-deps}

Always declare shared dependencies
in the workspace `[workspace.dependencies]` table
and reference them with `.workspace = true`
in member crates.

```toml
# In the workspace root Cargo.toml
[workspace.dependencies]
ostd = { version = "0.17.0", path = "ostd" }
bitflags = "2.6"

# In a member crate's Cargo.toml
[dependencies]
ostd.workspace = true
bitflags.workspace = true
```

### Macros and Attributes

#### Sort attributes and derive traits alphabetically (`alphabetical-attrs`) {#alphabetical-attrs}

When an item carries multiple outer attributes,
list non-derive attributes in **alphabetical order** by name
and place `#[derive(...)]` **last**.
Within `#[derive(...)]`,
list the traits **alphabetically** as well.

```rust
// Good — non-derive attributes sorted; derive is last with sorted traits
#[cfg(feature = "alloc")]
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Pod)]
pub struct Foo { ... }

// Bad — arbitrary ordering
#[derive(Debug, Default, Clone, Copy, Pod)]
#[cfg(feature = "alloc")]
#[repr(C)]
pub struct Foo { ... }
```

Placing `#[derive(...)]` last ensures
that derive macros always see the item
after all attribute macros
(e.g., `#[padding_struct]`, `#[pod_union]`)
have transformed it.
Derive helper attributes
(e.g., `#[serde(...)]`, `#[clap(...)]`)
stay immediately after `#[derive(...)]`.
Sorting the remaining attributes alphabetically
eliminates hesitation over placement
and reduces noise in diffs.

#### Use `#[expect(dead_code)]` with restraint (`expect-dead-code`) {#expect-dead-code}

In general, dead code should be avoided because
_(i)_ it introduces unnecessary maintenance overhead, and
_(ii)_ its correctness can only be guaranteed
by manual and error-prone review.

Dead code is acceptable only when all of these hold:

1. A _concrete case_ will be implemented in the future
   that turns the dead code into used code.
2. The semantics are _clear_ enough,
   even without the use case.
3. The dead code is _simple_ enough
   that both the committer and the reviewer
   can be confident it is correct without testing.
4. It serves as a counterpart to existing non-dead code.

For example, it is fine to add ABI constants
that are unused because the corresponding feature
is partially implemented.

#### Suppress lints at the narrowest scope (`narrow-lint-suppression`) {#narrow-lint-suppression}

When suppressing lints,
the suppression should affect as little scope as possible.
This makes readers aware
of the exact places where the lint is generated
and makes it easier for subsequent committers
to maintain the suppression.

```rust
// Good — each method is individually marked
trait SomeTrait {
    #[expect(dead_code)]
    fn foo();

    #[expect(dead_code)]
    fn bar();

    fn baz();
}

// Bad — the entire trait is suppressed
#[expect(dead_code)]
trait SomeTrait { ... }
```

There is one exception:
if it is clear enough
that every member will trigger the lint,
it is reasonable to expect the lint at the type level.

```rust
#[expect(non_camel_case_types)]
enum SomeEnum {
    FOO_ABC,
    BAR_DEF,
}
```

#### Prefer functions over macros (`macros-as-last-resort`) {#macros-as-last-resort}

Prefer functions and generics over macros.
Macros are powerful
but harder to understand, debug, test, and format.
Reach for a macro only when
the type system or generics cannot express
what you need
(e.g., variadic arguments, compile-time code generation,
or DSL syntax).

```rust
// Good — a generic function covers all types
fn align_up<T: Into<usize>>(val: T, align: usize) -> usize {
    let val = val.into();
    (val + align - 1) & !(align - 1)
}

// Bad — a macro where a function would suffice
macro_rules! align_up {
    ($val:expr, $align:expr) => {
        ($val + $align - 1) & !($align - 1)
    };
}
```

## Select Topics

### Concurrency and Races

Concurrency code is reviewed with extreme rigor.
Lock ordering, atomic correctness, memory ordering,
and race condition analysis are all demanded explicitly.

#### Establish and enforce a consistent lock order (`lock-ordering`) {#lock-ordering}

Acquiring two locks in different orders
from different code paths
is a potential deadlock.
Hierarchical lock order must be established and documented.

```rust
pub(super) fn set_control(
    self: Arc<Self>,
    process: &Process,
) -> Result<()> {
    // Lock order: group of process -> session inner -> job control
    let process_group_mut = process.process_group.lock();
    // ...
}
```

#### Never do I/O or blocking operations while holding a spinlock (`no-io-under-spinlock`) {#no-io-under-spinlock}

Holding a spinlock while performing I/O
or blocking operations is a deadlock hazard.
Use a sleeping mutex or restructure
to drop the lock first.

```rust
// Good — spinlock dropped before I/O
let data = {
    let guard = self.state.lock(); // state: SpinLock<...>
    guard.pending_data.clone()
};
self.device.write(&data)?;

// Bad — I/O while holding spinlock
let guard = self.state.lock(); // state: SpinLock<...>
self.device.write(&guard.pending_data)?;
```

#### Do not use atomics casually (`careful-atomics`) {#careful-atomics}

When multiple atomic fields
must be updated in concert, use a lock.
Only use atomics when a single value
is genuinely independent.

```rust
// Good — a lock protects correlated fields
struct Stats {
    inner: SpinLock<StatsInner>,
}
struct StatsInner {
    total_bytes: u64,
    total_packets: u64,
}

// Bad — two atomics that must be consistent
// but can be observed in an inconsistent state
struct Stats {
    total_bytes: AtomicU64,
    total_packets: AtomicU64,
}
```

#### Critical sections must not be split across lock boundaries (`atomic-critical-sections`) {#atomic-critical-sections}

Operations that must be atomic
(check + conditional action)
must happen under the same lock acquisition.
Moving a comparison outside the critical region
is a correctness bug.

```rust
// Good — check and action under the same lock
let mut inner = self.inner.lock();
if inner.state == State::Ready {
    inner.state = State::Running;
    inner.start();
}

// Bad — TOCTOU race: state can change
// between the check and the action
let is_ready = self.inner.lock().state == State::Ready;
if is_ready {
    self.inner.lock().state = State::Running;
    self.inner.lock().start();
}
```

### Defensive Programming

#### Use `debug_assert` for correctness-only checks (`debug-assert`) {#debug-assert}

Assertions verifying invariants
that should never fail in correct code
belongs in `debug_assert!`, not `assert!`.
`debug_assert!` is compiled out in release builds,
so the check catches bugs during development
without costing anything in production.

```rust
debug_assert!(self.align.is_multiple_of(PAGE_SIZE));
debug_assert!(self.align.is_power_of_two());
```

### Error Handling

#### Propagate errors with `?` (`propagate-errors`) {#propagate-errors}

Use the `?` operator
to propagate errors idiomatically.
In kernel code,
`.unwrap()` is rejected
wherever failure is a legitimate possibility.

```rust
// Good — propagate with ?
let tsc_info = cpuid.get_tsc_info()?;
let frequency = tsc_info.nominal_frequency()?;

// Bad — unwrap hides the failure path
let tsc_info = cpuid.get_tsc_info().unwrap();
```

### Logging

#### Use OSTD logging macros exclusively (`ostd-log-only`) {#ostd-log-only}

All OSTD-based crates must use the logging macros
provided by the `ostd::log` module:
`debug!`, `info!`, `notice!`, `warn!`, `error!`,
`crit!`, `alert!`, `emerg!`.
Import them via `use ostd::prelude::*`
or `use ostd::log::{info, warn, ...}`.

Do not use the third-party `log` crate directly.
OSTD provides a bridge that forwards messages
from third-party crates (e.g., `smoltcp`) that use `log`,
but first-party code must use OSTD's macros.

Custom output functions, `println!`,
and hand-rolled serial print macros
are not acceptable in production code.
Exception: code that runs before the logging subsystem
is initialized may use early-boot output helpers.

```rust
// Good
info!("VirtIO block device initialized: {} sectors", num_sectors);

// Bad — using the log crate directly
log::info!("VirtIO block device initialized: {} sectors", num_sectors);

// Bad — using println
println!("VirtIO block device initialized: {} sectors", num_sectors);
```

#### Choose appropriate log levels (`log-levels`) {#log-levels}

OSTD provides eight log levels matching the severity levels
described in `syslog(2)`:

| Level | Use for |
|-------|---------|
| `emerg!` | System is unusable; immediately before `abort()`. |
| `alert!` | Action must be taken immediately. |
| `crit!` | Critical conditions: unrecoverable resource exhaustion. |
| `error!` | Serious but recoverable failures: invariant violations, I/O errors. |
| `warn!` | Recoverable problems: fallback paths taken, deprecated usage detected. |
| `notice!` | Normal but significant events: CPU online, security feature activated. |
| `info!` | Routine informational events: subsystem initialization, configuration changes. |
| `debug!` | Development diagnostics: state transitions, intermediate values, per-packet tracing. |

Use `error!` for failures that the system can recover from.
Use `crit!` or `emerg!` only for failures immediately before a halt or abort.
A log statement that fires on every syscall
or every timer tick must use `debug!`.

#### Keep log prefixes consistent with the active logger (`log-prefix`) {#log-prefix}

Only define a `__log_prefix` macro
when the active logger consumes it
and it adds useful signal
beyond the default module, file, or line metadata.

If the current logger already prints
clear source-location context,
prefer not to add a redundant prefix macro.

Do not use manual bracket prefixes like `[IOMMU]` or `[Virtio]:`.

### Memory and Resource Management

#### Use RAII for all resource acquisition and release (`raii`) {#raii}

Resources — IRQ enable/disable state, port numbers,
file handles, DMA buffers, lock guards —
must use the `Drop` trait for automatic cleanup.
Manual `enable()`/`disable()` call pairs are rejected.

```rust
// Good — RAII guard ensures IRQs are re-enabled
fn disable_local() -> DisabledLocalIrqGuard { ... }

impl Drop for DisabledLocalIrqGuard {
    fn drop(&mut self) {
        enable_local_irqs();
    }
}

// Bad — caller can forget to re-enable
fn disable_local_irqs() { ... }
fn enable_local_irqs() { ... }
```

Prefer lexical lifetimes
so the Rust compiler inserts `drop` automatically,
rather than calling `drop()` manually.
When the default drop order is incorrect,
use explicit `drop()` calls.

### Performance

#### Avoid O(n) algorithms on hot paths (`no-linear-hot-paths`) {#no-linear-hot-paths}

System call dispatch, scheduler enqueue,
and frequent query operations
must not introduce O(n) complexity
where n is a quantity that can be large
(number of processes, number of file descriptors, etc.).
Demand sub-linear alternatives.

```rust
// Bad — O(n) scan on every enqueue
fn select_cpu(&self, cpus: &[CpuState]) -> CpuId {
    cpus.iter()
        .min_by_key(|c| c.load())
        .expect("at least one CPU")
        .id()
}

// Good — maintain a priority queue
// so selection is O(log n)
fn select_cpu(&self) -> CpuId {
    self.cpu_heap.peek().expect("at least one CPU").id()
}
```

#### Minimize unnecessary copies and allocations (`minimize-copies`) {#minimize-copies}

Extra data copies —
serializing to a stack buffer before writing,
cloning an `Arc` when a `&` reference suffices,
collecting into a `Vec` when an iterator would do —
should be avoided.

```rust
// Bad — unnecessary Arc::clone
fn process(&self, stream: Arc<DmaStream>) {
    let s = stream.clone();
    s.sync();
}

// Good — borrow when ownership is not needed
fn process(&self, stream: &DmaStream) {
    stream.sync();
}
```

#### No premature optimization without evidence (`no-premature-optimization`) {#no-premature-optimization}

Performance optimizations
must be justified with data.
Introducing complexity
to solve a non-existent problem is rejected.
If you claim a change improves performance,
show the numbers.

---

# Git Guidelines

### Write imperative, descriptive subject lines (`imperative-subject`) {#imperative-subject}

Write commit messages in imperative mood
with the subject line at or below 72 characters.
Wrap identifiers in backticks.

Common prefixes used in the Asterinas commit log:

- `Fix` — correct a bug
- `Add` — introduce new functionality
- `Remove` — delete code or features
- `Refactor` — restructure without changing behavior
- `Rename` — change names of files, modules, or symbols
- `Implement` — add a new subsystem or feature
- `Enable` — turn on a previously disabled capability
- `Clean up` — minor tidying without functional change
- `Bump` — update a dependency version

Examples:

```
Fix deadlock in `Vmar::protect` when holding the page table lock

Add initial support for the io_uring subsystem

Refactor `TcpSocket` to separate connection state from I/O logic
```

If the commit requires further explanation,
add a blank line after the subject
followed by a body paragraph
describing the _why_ behind the change.

### One logical change per commit (`atomic-commits`) {#atomic-commits}

Each commit should represent one logical change.
Do not mix unrelated changes in a single commit.
When fixing an issue discovered during review
on a local or private branch,
use `git rebase -i` to amend the commit
that introduced the issue
rather than appending a fixup commit at the end.

### Separate refactoring from features (`refactor-then-feature`) {#refactor-then-feature}

If a feature requires preparatory refactoring,
put the refactoring in its own commit(s)
before the feature commit.
This makes each commit easier to review and bisect.

### Keep pull requests focused (`focused-prs`) {#focused-prs}

Keep pull requests focused on a single topic.
A PR that mixes a bug fix, a refactoring,
and a new feature is difficult to review.

Ensure that CI passes before requesting review.
If CI fails on an unrelated flake,
note it in the PR description.

---

# Testing Guidelines

### Add regression tests for every bug fix (`add-regression-tests`) {#add-regression-tests}

When a bug is fixed,
a test that would have caught the bug should accompany the fix.
Include a reference to the issue number
in a comment so future readers
can recover the original context.

### Test user-visible behavior, not internals (`test-visible-behavior`) {#test-visible-behavior}

Tests should validate observable, user-facing outcomes.
Prefer testing through public APIs
rather than exposing internal constants in test code.

Name tests after the behavior or specification concept being verified,
not after internal implementation details.
Using kernel-internal names in user-space regression tests
creates unnecessary coupling.

### Use assertion macros, not manual inspection (`use-assertions`) {#use-assertions}

Use language- or framework-provided assertion helpers
instead of printing values and manually inspecting output.
Assertions provide clear failure messages
and make tests self-checking.

### Clean up resources after every test (`test-cleanup`) {#test-cleanup}

Always clean up resources after a test:
close file descriptors, unlink temporary files,
and call `waitpid` on child processes.
Leftover resources can cause flaky failures
in subsequent tests.

```c
// Good — cleanup after use
int fd = open("/tmp/test_file", O_CREAT | O_RDWR, 0644);
// ... test logic ...
close(fd);
unlink("/tmp/test_file");
```

---

# Assembly Guidelines

## Sections

### Use the correct section directive (`asm-section-directives`) {#asm-section-directives}

For built-in sections, use the short directive (e.g., `.text`).
For custom sections,
use the `.section` directive with flags and type
(e.g., `.section ".bsp_boot", "awx", @progbits`).

A blank line should follow each section definition
to visually separate it from the code that follows.

```asm
.section ".bsp_boot.stack", "aw", @nobits

boot_stack_bottom:
    .balign 4096
    .skip 0x40000  # 256 KiB
boot_stack_top:
```

### Place code-width directives after the section definition (`asm-code-width`) {#asm-code-width}

In x86-64, if an executable section contains only 64-bit code,
place the `.code64` directive directly after the section definition.
The same applies to `.code32` for 32-bit code.
In mixed sections, treat `.code64` and `.code32`
as function attributes (see below).

```asm
.text
.code64

.global foo
foo:
    mov rax, 1
    ret
```

## Functions

### Place attributes directly before the function (`asm-function-attributes`) {#asm-function-attributes}

Function attributes (`.global`, `.balign`, `.type`)
should be placed directly before the function label
and should not be indented.
Prefer `.global` over `.globl` for clarity.

```asm
.balign 4
.global foo
foo:
    mov rax, 1
    ret
```

### Add `.type` and `.size` for Rust-callable functions (`asm-type-and-size`) {#asm-type-and-size}

Functions that can be called from Rust code
should include the `.type` and `.size` directives.
This gives debuggers a better understanding of the function.

```asm
.global bar
.type bar, @function
bar:
    mov rax, 2
    ret
.size bar, .-bar
```

This does not apply to boot entry points,
exception trampolines, or interrupt trampolines —
they may not fit the typical definition of "function"
and their sizes may be ill-defined.

### Use unique label prefixes to avoid name clashes (`asm-label-prefixes`) {#asm-label-prefixes}

A Rust crate is a single translation unit,
so `global_asm!` labels in different modules
within the same crate share the same global namespace.
Add custom prefixes to labels to avoid name clashes
(e.g., `bsp_` for BSP boot code, `ap_` for AP boot code).

```asm
# Good — prefixed to avoid clashes
bsp_boot_stack_top:
ap_boot_stack_top:

# Bad — generic names risk duplication
boot_stack_top:
```

### Prefer `.balign` over `.align` (`asm-prefer-balign`) {#asm-prefer-balign}

The `.align` directive's behavior varies across architectures —
on some it specifies a byte count,
on others a power of two.
Use `.balign` for unambiguous byte-count alignment.

```asm
# Good — unambiguous
.balign 4096

# Bad — architecture-dependent meaning
.align 12
```

---

# How Guidelines Are Written

The guidelines in this collection reflect
a set of widely-recognized **philosophy** and **principles**
for writing high-quality software.
Three books have influenced the guidelines the most:
1. [*The Art of Readable Code*](https://www.oreilly.com/library/view/the-art-of/9781449318482/)
2. [*Clean Code*](https://www.oreilly.com/library/view/clean-code-a/9780136083238/)
3. [*Code Complete*](https://stevemcconnell.com/books/)

The guidelines are derived from **code review experience**,
where we point out code smells, observe anti-patterns, and fix recurring bugs.
By the time the initial version of these guidelines was formulated,
Asterinas had seen thousands of code reviews.
We collected the historical review comments
as a [dataset](https://github.com/asterinas/pr-review-analysis)
and used it as both inspiration and evidence.

The remainder of this section moves from values to practice:
Philosophy captures foundational beliefs about what makes code understandable and maintainable;
Principles turns those beliefs into design-level rules that guide everyday decisions;
Quality Criteria defines how individual guidelines are written and accepted.

## Philosophy {#philosophy}

### Minimize time to understand {#minimize-time-to-understand}

Code should be written to minimize the time
it would take someone else to fully understand it.
This is the fundamental theorem of readability
and the single most important measure
of code quality in this project.
"Someone else" includes your future self.

Code is read far more often than it is written.
If a technique makes code shorter
but harder to follow at a glance,
choose clarity over brevity.

### Managing complexity is the primary technical imperative {#managing-complexity}

No one can hold an entire modern program in their head.
The purpose of every technique in software construction —
decomposition, naming, encapsulation, abstraction —
is to break complex problems into simple pieces
so that you can safely focus on one thing at a time.

### Craftsmanship and care {#craftsmanship}

Clean code looks like it was written by someone who cares.
Professionalism means never knowingly leaving a mess.
The only way to go fast is to keep the code clean at all times.

### Continuous improvement {#continuous-improvement}

Leave code cleaner than you found it.
Small, steady improvements —
renaming a variable, extracting a function,
eliminating duplication —
prevent code from rotting over time.

## Principles {#principles}

### Single Responsibility {#single-responsibility}

Each module, type, or function
should have one, and only one, reason to change.
If you cannot describe what a unit does
without the words "and," "or," or "but,"
it has too many responsibilities.

### Don't Repeat Yourself (DRY) {#dry}

Every piece of knowledge
should have a single, unambiguous representation.
Duplication harms readability and maintainability.
When the same pattern appears three or more times,
eliminate the duplication (e.g., adding a helper function).

### Information Hiding {#information-hiding}

Hide design decisions behind well-defined interfaces.
A module's public surface should contain
only what its consumers need.
Internal data structures, helper types,
and bookkeeping fields should remain private.

### Open for Extension, Closed for Modification {#open-closed}

Stable modules and APIs should be
open to extension
but closed to breaking modification.
Prefer adding new behavior
through existing interfaces
(traits, enums, and pluggable components)
instead of repeatedly editing established call paths.
Do not introduce extension points preemptively;
add them when there is a concrete extension need.

### Least Surprise {#least-surprise}

Functions, types, and APIs should behave
as their names and signatures suggest.
When an obvious behavior is not implemented,
readers lose trust in the codebase
and must fall back on reading implementation details.

### Loose Coupling, Strong Cohesion {#coupling-cohesion}

Connections between modules should be
small, visible, and flexible.
Within a module, every part should contribute
to a single, well-defined purpose.

### Consistency {#consistency}

Do similar things the same way throughout the codebase.
Consistency reduces surprise and cognitive load
even when neither approach is objectively superior.
When a convention already exists, follow it;
do not introduce a competing convention
without compelling justification.

### Test as the Source of Confidence {#test-as-specification}

Tests exist to make change safe.
A comprehensive test suite should give developers confidence
that a passing run means the system works
and a failing run pinpoints what broke.
Every test must earn its place
by increasing that confidence.
A test that does not —
flaky tests, tautological assertions,
tests coupled to implementation details —
is worse than no test at all.

### Rust-Native Approach {#rust-native}

Asterinas is inspired by Linux but is not a C port.
The language shapes how we think about problems:
where C code relies on conventions and manual discipline
(return-code checking, paired init/cleanup, header-file contracts),
Rust offers compiler-enforced, zero-cost abstractions
(the `?` operator, RAII, trait bounds).

Learn from Linux's design, not its idioms.
The result should read like idiomatic Rust,
not like C written in Rust syntax.

## Quality Criteria {#quality-criteria}

Every guideline carries a **descriptive short name** in kebab-case
(e.g., `explain-variables`, `lock-ordering`).
Short names are kept **intact** even as the guidelines evolve
and should be used when referencing guidelines in code reviews.

A guideline is accepted into this collection
when it satisfies all four quality criteria:

1. **Concrete** —
   Framed as an actionable item with an illustrating example when possible.
2. **Concise** —
   Kept short; we do not want to intimidate readers.
3. **Grounded** —
   Opinionated or non-obvious guidelines should include a "See also" line
   with supportive materials (literature, PR reviews, codebase examples).
4. **Relevant** —
   Included only if it has codebase examples,
   prevents a past bug,
   or matches anti-patterns observed in code reviews.

When present, the **"See also" line** lists sources in the order:
literature; PR reviews; codebase examples.
Not every guideline needs all three;
for strongly opinionated or non-obvious guidelines,
include the line by default.

Do not add a guideline whose only value
is mechanical enforcement already provided by automated tools
such as [rustfmt](https://github.com/rust-lang/rustfmt) and [clippy](https://github.com/rust-lang/rust-clippy).
If a tool-enforced convention appears frequently in review
or needs project-specific rationale,
keep a short explanatory guideline and point to the tool configuration.

---

> **For more detailed coding guidelines**, see the full documents under [`docs/coding-guidelines-cn/`](../../../../docs/coding-guidelines-cn/).
> The Chinese translations provide the complete, expanded version of each guideline section with additional examples and explanations.
