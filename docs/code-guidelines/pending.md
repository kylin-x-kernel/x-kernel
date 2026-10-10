# Pending Dylint rules

Rules from `docs/ai/skills/code-guidelines/` that a Dylint lint in
`tools/lints` can check. Rules that clippy or rustc can already express
belong to the workspace-lints track, not here.

Until its lint lands, each row's original text is still a valid rule.
Landing a lint means: implement it, add the `CG-<nnn>-<descriptive>.md`
rule file (carrying the text and the judgment column below), clear the
backlog, and delete the row.

- Source `file:line` refers to `docs/ai/skills/code-guidelines/` just
  before this list was created.
- Backlog numbers are grep estimates, not lint runs; measure before landing.
- Coverage: *full* means the lint replaces the text; *partial* means the
  lint checks the mechanical part and the judgment column says what the
  rule file and review must still carry (usually what an `allow` reason
  has to justify).

## To migrate (original text removed from the skill)

| Lint | Original text (verbatim) | Source | Coverage | Judgment kept / exception reasons | Backlog | Cost | Priority |
|---|---|---|---|---|---|---|---|
| `user_copy_without_abi_contract` | "copying raw bytes into arbitrary `T` without a `UserRead`-style validity contract." | `unsafety.md:189` | full for user memory | Write side (`write_vm_mem`, raw-pointer `write_vm` / `write_vm_slice` need `UserWrite`) has no skill sentence; state it in the rule file. `unittest_support` generic helpers need an item-level reason. If `UserRead` / `UserWrite` move into `osvm` and bound its generic copy APIs, rustc covers the write side; then drop that half. | Write side: `loop_info` (`LOOP_GET_STATUS`, leaks kernel stack bytes, reproduced) and `FixScreenInfo` (`FBIOGET_FSCREENINFO`, padded); padding-free but unguarded `Termios`, `Termios2`, `WindowSize`, `VarScreenInfo`, `rtc_time`, `cmsghdr`, `getsockopt` `put<T>`; ~14 scalar sites. `msghdr` / `mmsghdr` are cleared by the #IKIV28 fix. | small | high |
| `unregistered_static_lock` (extends CG-001 to `ksync::Mutex` / `ksync::RwLock`) | "When `stats` is off, `static_lock!` is an identity macro and `Mutex::new` / `RwLock::new` stay `const fn`. Wrapping static Mutex and RwLock items in `static_lock!` remains the right form."<br>"Do not use it as the default for ordinary statics."<br>"`static Mutex` / `RwLock` without `static_lock!`;"<br>"static `Mutex` / `RwLock` items use `static_lock!`,"<br>"For new or moved `Mutex` / `RwLock` / spin statics, also check `concurrency.md` lock construction (`static_lock!` vs `::new`)."<br>"The crate that owns the static must depend on `ksync` (Mutex / RwLock) or `kspin` (spin types). The macro resolves those crate names from `Cargo.toml`." | `concurrency.md:81-83`<br>`concurrency.md:104`<br>`concurrency.md:170`<br>`SKILL.md:121`<br>`SKILL.md:96-97`<br>`concurrency.md:85-87` | full | `const_new(RawMutex::with_config(..), v)` stays out of scope (the macro only rewrites `new`). Widen CG-001 or give it a new id. Doctests in `task/ksync/src/lib.rs:20,48` violate the rule and are not linted. | 0 | small (reuses CG-001) | high |
| `derive_not_last` | "place `#[derive(...)]` after other outer attributes;"<br>"attributes or derives ordered inconsistently with the project convention;" | `modules-and-attributes.md:32`<br>`modules-and-attributes.md:59` | full (with `unsorted_attributes`) | None. Needs a pre-expansion pass (`derive` is gone from HIR). | ~102 | small | high |
| `untracked_runtime_lock` | "and heap locks use `::new` rather than `const_new` unless an untracked const initializer is required;"<br>"If a constructor is only used at runtime, drop `const` so it can keep `Mutex::new` and remain tracked."<br>"Use `Mutex::const_new(RawMutex::new(), val)` (and the RwLock equivalent) only when a `const` initializer is required and tracking is acceptable to lose." | `SKILL.md:122-123`<br>`concurrency.md:97-98`<br>`concurrency.md:102-103` | partial | A `const fn` constructor may use `const_new` only if it is really called from a const initializer; the reason must name that caller. `with_config` raw locks are out of scope. | 0 | small | medium |
| `undocumented_unsafe_attribute` | "treating `#[unsafe(naked)]`, `#[unsafe(no_mangle)]`, or linker-section placement as self-justifying." | `unsafety.md:437-438` | full (presence of a `// SAFETY:` justification) | Whether the justification names the real entry-state assumptions stays with review (see `unsafety.md:425-427`, kept). First scope: `naked`, `no_mangle`, `export_name`. | ~88 (`naked` 19, `no_mangle` 68, `export_name` 1); `link_section` +109 | small (early pass + source text) | medium |
| `unsorted_attributes` | "keep attributes ordered predictably;"<br>"keep derived traits ordered consistently;" | `modules-and-attributes.md:31`<br>`modules-and-attributes.md:33` | full | The convention is alphabetical (non-derive attributes by name, traits inside `derive` by last path segment), as in `docs/coding-guidelines/rust-guidelines/language-items/macros-and-attributes.md`; the rule file must state it and the tie-break for `cfg_attr`. | ~641 derive lists, ~109 attribute groups | small (shares the `derive_not_last` pass) | medium |
| `free_function_import` | "prefer importing a parent module and qualifying free-function calls rather than importing free functions directly by name;"<br>"direct free-function imports that remove call-site context;" | `modules-and-attributes.md:25-26`<br>`modules-and-attributes.md:56` | full (with an allowlist) | Allowlisted core helpers (`min`, `max`, `swap`, `replace`, `take`, `black_box`, `poll_fn`); decide up front whether test modules may import helpers from `super`. | ~1,300 in ~450 files | small | medium |
| `vec_shift_in_loop` | "`Vec::remove`/`Vec::insert` inside loops," | `performance-and-resources.md:70` (first half; "`Vec` growth on hot paths" kept) | partial | The reason must show the vector is small or bounded, so the O(n) shift does not matter. | ~4–8 | small | low |
| `static_mut_option` | "open-coded `static mut Option<T>` initialization;" | `unsafety.md:268` | full | None. | 0 | small | low |

## Supporting lints (original text kept)

The lint checks one side of the rule; the rest is a separate requirement,
so the text stays in the skill.

| Lint | Related text (kept) | Source | What the lint checks | Backlog | Cost | Priority |
|---|---|---|---|---|---|---|
| `unsound_user_abi_impl` | "`UserRead` / `UserWrite` for by-value syscall ABI objects;"<br>"`repr(C)`/layout-proven ABI structs for external binary formats;" | `unsafety.md:180`<br>`unsafety.md:309` | The `UserWrite` / `UserRead` `# Safety` contracts from the layout: no implicit padding for `UserWrite`, no niche for `UserRead`, `repr(C)` / `transparent` / `packed` or primitive. Derive-generated impls included. A compile-time check in `derive(UserWrite)` (size equals the field sum, every field `UserWrite`) would cover concrete derived structs; the lint stays authoritative for manual impls, unions, and generics. | ~126 impls in scope. `sysinfo`, `epoll_event`, `flock64`, `IpcPerm` and four DRM ioctl structs are cleared by the #IKIV28 fix. Open: unions `k_sigval` / `k_sigevent` carry `UserWrite` though a 4-byte member leaves bytes uninitialized; `k_siginfo` relies on all-zero construction. | small | high |
| `blocking_call_under_spinlock` | "do not perform I/O or blocking work while holding a spinlock;"<br>"do not acquire blocking synchronization primitives such as `ksync::Mutex`, `RwLock`, `Semaphore`, or `WaitQueue` while holding a spinlock;"<br>"if a path may reach `block_on`, `wait`, `sleep`, event listeners, scheduler operations, or code that can return `WouldBlock` and then wait, the path must not hold a spinlock;"<br>"blocking operations under spinlocks;"<br>"sleepable locks acquired while a spinlock, IRQ-disable guard, or preempt-disable guard is held;" | `concurrency.md:9-12`<br>`concurrency.md:42-44`<br>`concurrency.md:162`<br>`concurrency.md:164-165` | Direct calls to known sleeping entry points in the same function while a `kspin` lock guard or IRQ / preempt guard may be live (MIR init dataflow). Blocking reached through callees stays with review. | ≈0 direct | large (MIR dataflow) | medium |
| `exported_foreign_item` | "a narrow `unsafe extern` declaration layer;"<br>"exposing foreign ownership obligations to unrelated callers." | `unsafety.md:399`<br>`unsafety.md:408` | `extern` block items must not be reachable outside their crate. | 3 per target (`arch/kcpu` user-copy primitives) | small | low-medium |
| `untyped_error_result` | "return typed errors that match the layer's contract;" | `error-handling-and-logging.md:11` | `Result` error type must not be `()`, `&str` or `String`. | ~29 (mostly boot / ELF tools) | small | medium-low |
| `unitless_time_integer` | "encode units in names when the type does not;"<br>"use `timeout_ns`, not `timeout`" | `naming-and-structure.md:12`<br>`naming-and-structure.md:24` | Integer bindings named as time quantities carry a unit. Size and count units stay with review. | ~18 | small | low-medium |
| `nondeterministic_unit_test` | "wall-clock thresholds such as "must finish within N ms";"<br>"median/min/max timing assertions;"<br>"fixed retry loops that assume "N yields is enough";"<br>"unit tests that rely on timing, fixed yield counts, or unscoped global state;" | `git-and-testing.md:73-75`<br>`git-and-testing.md:106` | Sleeps, clock comparisons and fixed-count yield loops inside `def_test` / `cfg(unittest)` code. Needs a policy call on watchdog loops first. | ~20 | small | low-medium |

## Evaluated, not Dylint

- `concurrency.md:77-79`, `:95`, `:171-172` (lock construction must compile
  with `stats`; no `Mutex::new` in const context): rustc already rejects
  these with `KFEAT_LOCK_STAT=y`. The aarch64 Jenkins dylint branch rescans
  with that option, which covers them without lint code.
- `assembly.md:17`, `:26` (`.balign` over `.align`): a text check over `*.S`
  and `*.rs` covers more files than a lint; backlog 0.
- `api-design.md:19` (enums instead of trait objects for closed sets): the
  lint can prove a trait is closed, but most hits would be justified
  vtable designs.
- Holding a `kspin` guard across `.await`: `clippy::await_holding_invalid_type`
  with `await-holding-invalid-types` configuration.
- Test modules declared with `#[mod_test]` instead of a bare
  `#[cfg(unittest)] mod`: a good lint, but no skill sentence states it yet.
