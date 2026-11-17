# Pending Dylint rules

Rules from `docs/ai/skills/code-guidelines/` that a Dylint lint in
`tools/lints` can check. Rules that clippy or rustc can already express
belong to the workspace-lints track, not here.

Until its lint lands, each row's original text is still a valid rule.
Landing a lint means: implement it, add the `CG-<nnn>-<descriptive>.md`
rule file (carrying the text and the judgment column below), clear the
backlog, and delete the row.

- Source `file:line` refers to `docs/ai/skills/code-guidelines/` just
  before this list was created. L1–L3 rows added in the second AI batch
  use the part 8 snapshot `2f04ea0e9` instead; older rows retain their coordinates.
- Backlog numbers are grep estimates, not lint runs; measure before landing.
- Coverage: *full* means the lint replaces the text; *partial* means the
  lint checks the mechanical part and the judgment column says what the
  rule file and review must still carry (usually what an `allow` reason
  has to justify).

## To migrate (original text removed from the skill)

| Lint | Original text (verbatim) | Source | Coverage | Judgment kept / exception reasons | Backlog | Cost | Priority |
|---|---|---|---|---|---|---|---|
| `free_function_import` | "prefer importing a parent module and qualifying free-function calls rather than importing free functions directly by name;"<br>"direct free-function imports that remove call-site context;" | `modules-and-attributes.md:25-26`<br>`modules-and-attributes.md:56` | full (with an allowlist) | Allowlisted core helpers (`min`, `max`, `swap`, `replace`, `take`, `black_box`, `poll_fn`); decide up front whether test modules may import helpers from `super`. | ~1,300 in ~450 files | small | medium |
| `assertion_like_bool_names` | "keep boolean names assertion-like (`is_*`, `has_*`, `can_*`, `should_*`, `needs_*`);"<br>"negated boolean names;" | `naming-and-structure.md:13-14`<br>`naming-and-structure.md:43` | full (resolved bool names; lexical prefixes) | Resolved bool bindings, fields and bool-returning function names use positive is_/has_/can_/should_/needs_ prefixes; reject negated not_/no_ forms. Preserve external ABI/trait/generated names with focused exceptions. Does not infer predicate correctness; CG-030 counts bool parameters instead. | not measured; resolve and measure before landing | small-medium | medium |
| `ordinary_static_mut` | "`static mut` for ordinary shared state;" | `unsafety.md:131` | partial (non-Option declarations) | Start SHOULD for resolved non-Option static mut declarations used as ordinary shared state; exclude Option<T>, already the `static_mut_option` row. Focused boot/FFI or enforced confinement exceptions must establish the actual access model. UnsafeCell alone is not synchronization. | not measured; resolve and measure before landing | small | medium |
| `lock_stat_record_api_bypass` | "Do not write private counter fields."<br>"lock-stat counters updated by writing private fields instead of `record_acquisitions` / `record_contentions`." | `concurrency.md:69`<br>`concurrency.md:126-127` | partial (record-API bypass) | Resolve counter-field modifying operations, including atomic mutation, outside approved record_acquisitions/record_contentions implementations. Field privacy does not exclude sibling methods in the owning module. CG-040 owns one-per-success/actual-contention branch accounting; CG-001–003 own registration/construction. | not measured; resolve and measure before landing | medium | medium |

## Supporting lints (original text kept)

The lint checks one side of the rule; the rest is a separate requirement,
so the text stays in the skill.

| Lint | Related text (kept) | Source | What the lint checks | Backlog | Cost | Priority |
|---|---|---|---|---|---|---|
| `blocking_call_under_spinlock` | "do not perform I/O or blocking work while holding a spinlock;"<br>"do not acquire blocking synchronization primitives such as `ksync::Mutex`, `RwLock`, `Semaphore`, or `WaitQueue` while holding a spinlock;"<br>"if a path may reach `block_on`, `wait`, `sleep`, event listeners, scheduler operations, or code that can return `WouldBlock` and then wait, the path must not hold a spinlock;"<br>"blocking operations under spinlocks;"<br>"sleepable locks acquired while a spinlock, IRQ-disable guard, or preempt-disable guard is held;" | `concurrency.md:9-12`<br>`concurrency.md:42-44`<br>`concurrency.md:162`<br>`concurrency.md:164-165` | Direct calls to known sleeping entry points in the same function while a `kspin` lock guard or IRQ / preempt guard may be live (MIR init dataflow). Blocking reached through callees stays with review. | ≈0 direct | large (MIR dataflow) | medium |
| `nondeterministic_unit_test` | "wall-clock thresholds such as "must finish within N ms";"<br>"median/min/max timing assertions;"<br>"fixed retry loops that assume "N yields is enough";"<br>"unit tests that rely on timing, fixed yield counts, or unscoped global state;" | `git-and-testing.md:73-75`<br>`git-and-testing.md:106` | Sleeps, clock comparisons and fixed-count yield loops inside `def_test` / `cfg(unittest)` code. Needs a policy call on watchdog loops first. | ~20 | small | low-medium |

## Evaluated, not Dylint

- `concurrency.md:77-79`, `:95`, `:171-172` (lock construction must compile
  with `stats`; no `Mutex::new` in const context): rustc already rejects
  these with `KFEAT_LOCK_STAT=y`. The aarch64 Jenkins dylint branch rescans
  with that option, which covers them without lint code. Lock initialization
  must compile with that feature enabled; `Mutex::new` / `RwLock::new` are not
  const under `stats`, including wrapper calls from const fn or static
  initializers. Feature-off checks are insufficient.
- `assembly.md:17`, `:26` (`.balign` over `.align`): a text check over `*.S`
  and `*.rs` covers more files than a lint; backlog 0.
- `api-design.md:19` (enums instead of trait objects for closed sets): the
  lint can prove a trait is closed, but most hits would be justified
  vtable designs.
- Holding a `kspin` guard across `.await`: `clippy::await_holding_invalid_type`
  with `await-holding-invalid-types` configuration.
- Test modules declared with `#[mod_test]` instead of a bare
  `#[cfg(unittest)] mod`: a good lint, but no skill sentence states it yet.
