# Code Guidelines

Use this skill when an AI agent needs to write,
edit, or review Rust kernel code in the X-Kernel repository.

This skill is the canonical shared entry point
for code-writing and code-review conventions.
The detailed topic rules live in sibling files
under this same directory.

## Scope

This skill covers:

- naming and API shape;
- comments, rustdoc, and module-level documentation;
- `unsafe` usage and safety reasoning;
- visibility, modules, attributes, and workspace dependencies;
- syscall and current-context boundary semantics;
- concurrency, lock ordering, and atomics;
- error handling;
- logging;
- resource management, data structure selection,
  and performance-sensitive code review.

This skill does not replace:

- project build/run/test commands
  (`docs/ai/skills/build-workflow/SKILL.md`);
- per-crate design and security documentation generation
  (`docs/ai/skills/module-docs/SKILL.md`).

This skill primarily targets Rust code-writing and code review.
It also includes lightweight extension topics for:

- commit structuring and regression-test expectations;
- assembly style for `.S` and `global_asm!`
  when a code change touches them.

## Topic Files

Load only the files relevant to the change:

- `naming-and-structure.md`
- `comments-and-rustdoc.md`
- `api-design.md`
- `boundaries-and-context.md`
- `modules-and-attributes.md`
- `unsafety.md`
- `concurrency.md`
- `error-handling-and-logging.md`
- `performance-and-resources.md`
- `git-and-testing.md`
- `assembly.md`

Treat `git-and-testing.md` and `assembly.md`
as extension topics.
For ordinary Rust code changes,
the core topic files above them are the primary path.

## Writing Workflow

When writing or editing Rust kernel code:

1. Identify whether the change touches `unsafe`,
   raw pointers, FFI, uninitialized storage,
   shared state, logging, public APIs, or hot paths.
2. Load the matching topic files from this skill directory.
3. Apply the mandatory rules from those topic files.
4. Re-read the patch specifically for:
   naming,
   boundary validation,
   current-context assumptions,
   `unsafe` reasoning,
   unsafe scope minimization,
   lock scope,
   error propagation,
   and logging quality.
5. If the change alters public behavior or module contracts,
   update rustdoc and any crate-local docs that describe that behavior.

## Review Workflow

When reviewing Rust kernel code:

1. Check whether names reflect the real semantics and units.
2. Check whether comments explain intent rather than paraphrasing code.
3. If the patch touches `unsafe`, load `unsafety.md`
   and review against its scenario catalog,
   `SAFETY:` comment standard, and review questions.
4. Check every `unsafe` block, `unsafe fn`, and `unsafe impl`
   for explicit invariants and sufficient audit surface.
5. Check whether raw-pointer, FFI, and `MaybeUninit`
   usage is modeled with the narrowest sound pattern available.
6. Check whether lock scope, lock order, and wake/block behavior are safe.
7. Check whether syscall boundaries and current-context assumptions are explicit.
8. Check whether `?`, typed errors, and logging levels are used appropriately.
9. Check whether modules, visibility, macros, and attributes stay disciplined.
10. Check whether the code introduces avoidable hot-path scans,
   copies, allocations, or atomics.

## Review Checklist

Before considering a code change aligned with this skill, verify:

- names are descriptive, accurate, and unit-aware where needed;
- comments and rustdoc explain intent and contracts;
- current-context semantics are explicit where the API depends on them;
- every `unsafe` site has a concrete safety argument;
- every `unsafe` block has a nearby `SAFETY:` comment that
  names the real invariants rather than using placeholders;
- `unsafe fn` bodies do not silently rely on implicit whole-body unsafety;
- `unsafe` review uses the scenario catalog from `unsafety.md`
  instead of ad hoc reasoning;
- uninitialized storage uses `MaybeUninit<T>` or an equivalent explicit model;
- raw-pointer and FFI code states validity, alignment,
  lifetime, aliasing, and ABI assumptions where relevant;
- shared-state code has sane lock scope and lock ordering;
- boundary validation happens at subsystem edges;
- modules, visibility, imports, and attributes stay narrowly scoped;
- error paths use `Result` and `?` rather than hidden panics;
- logging uses crate-standard prefixes and appropriate severity;
- container choices match their access pattern; queues needing middle
  removal, stable element addresses, or IRQ-safe enqueue are not built
  on growable `Vec` bookkeeping, and linked-list needs use the
  project-standard intrusive `linked_list_r4l`;
- the patch does not introduce obvious hot-path regressions.

## Canonical Human References

The repository still keeps the full human-oriented guideline corpus under:

- `docs/coding-guidelines/`
- `docs/coding-guidelines-cn/`

Those directories are background references,
not the primary execution path for this skill.
