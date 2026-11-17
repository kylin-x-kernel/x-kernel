# xkernel lints

Dylint library holding the repository's custom lints. Each lint enforces a
rule in `docs/code-guidelines/`. The crate builds one cdylib that
`make dylint` loads to scan the configured kernel.

## Prerequisites

The lint toolchain and its components are pinned in `rust-toolchain.toml`
here; `cargo-dylint` and `dylint-link` are pinned by `DYLINT_VERSION` in
`xtask/xkmake/src/hygiene.rs`. Install the toolchain with the kernel target
of each platform you scan, then the tools:

```bash
toolchain="$(sed -n 's/^channel = "\(.*\)"$/\1/p' tools/lints/rust-toolchain.toml)"
rustup toolchain install "$toolchain" --profile minimal \
    --component rustc-dev --component llvm-tools --component rustfmt \
    --target aarch64-unknown-none-softfloat --target x86_64-unknown-none
make install-tools
```

## Running

```bash
make dylint          # scan the kernel selected by .config
make dylint XKMAKE_ARGS=--fix  # apply machine suggestions in configured local packages
make dylint_test     # run every lint's UI test
make dylint XKMAKE_ARGS="-- --message-format=json"
make dylint FACTS_OUT=<dir>   # also export code facts (see below)
```

The fix mode selects local packages from Cargo's application compilation unit
graph, so Cargo applies suggestions in dependencies of `entry` without
selecting inactive platform or development packages. It checks that the fix
build resolves the same package features as the scan. Dry-run reports how
package selection will be resolved at run time without starting Cargo.
Re-run fix mode after moving derives, because that can expose a derive list or
attribute group that still needs sorting.

A scan only sees code that the configuration compiles; CI scans the
aarch64 and x86_64 `qemu_defconfig` configurations. Add the platform's
kernel target to the toolchain before scanning another configuration.

No lint checks a vendored crate, one whose manifest sits directly under
`vendor/` (see `docs/code-guidelines/README.md`, "Vendored code").
`register_lints` registers only the facts export for it, so the dependency
facts scan and `--fix`, which both reach these path dependencies, report and
rewrite nothing there.

`xkmake` builds this library once into `<target>/tools/lints` and passes
it to `cargo dylint --lib-path`. The build ignores the rustflags of the
kernel configuration (the generated `.cargo/.xconfig.toml`), so switching
configurations, for example to `UNITTEST=y`, does not relink it; Dylint
records the library as a file dependency of every checked crate, and a
relink would re-check them all. It is rebuilt when its sources,
`Cargo.lock` or the toolchain change. `make dylint_test` uses the same
target directory.

The first local run builds the Dylint driver into `$DYLINT_DRIVER_PATH`
(default `~/.dylint_drivers`). The CI image sets `DYLINT_DRIVER_PATH` to a
driver prebuilt for the pinned toolchain.

## Standard Box in kernel tests

`std_box_in_kernel_tests` implements CG-049-no-std-box-in-kernel-tests at
`deny` level. It is active only with `--cfg unittest`. Within such a build,
it checks source files with an exact `ut-tests` path component and inline
items, associated items, expressions, local declarations, fields, or enum
variants guarded by outer `#[cfg(unittest)]` or `#[cfg(test)]`.
Positive `all`/`any` combinations are recognized when they require a test
condition. An inner crate-level `#![cfg(unittest)]` does not make ordinary
production items test code. Ordinary host unit/integration tests and tools
built without `--cfg unittest` are outside the rollout.

An early pass records test-gated source spans before expansion; a late pass
checks resolved types and associated functions. Standard `Box` is identified
by rustc's Box type identity, including renamed imports, type aliases and
qualified paths. Calls returning `Box` also report, including method and
UFCS forms of `into_boxed_slice` and `into_boxed_str`. Imports themselves do
not report, and unrelated user types named `Box` are allowed. Exceptions for
existing production interfaces belong on the narrowest containing item,
using `allow(unknown_lints, std_box_in_kernel_tests, reason =
"CG-049-no-std-box-in-kernel-tests: test scenario: <specific contract>")`
and the CG-024 `clippy::allow_attributes` expectation wrapper.

This is not ownership dataflow analysis: associated-type projections,
inferred moves of existing owners, and helper results wrapping a Box in
another type can escape detection when no checked Box path or direct
Box-valued call appears. Exception scope and justification remain review
requirements; rustc can technically honor broader allowances.

External macro implementation spans are excluded so `vec!`'s internal
standard `Box` does not trigger the rule. Source-spanned arguments such as
`vec![Box::new(value)]` and pass-through macro arguments still report.
Local macro implementations report at their invocation when invoked in UT
scope, including macros defined in production source. External macros that
construct a Box, and user tokens whose original spans a procedural macro
replaces with external expansion spans, are outside this check. Review
those macro definitions separately. The scope collector does not infer
test-only reachability, handle `cfg_attr` or negated logical equivalences,
or follow a test-gated file module outside `ut-tests/` into its source file.
Test attributes alone (`#[test]` or `#[unittest::def_test]`) do not establish
scope without a recognized cfg gate or a `ut-tests/` source path.
Inactive cfg branches are not type checked and cannot report.

The embedded UI tests exercise production neighbors in a unittest-enabled
crate, inline test helpers, gated locals/fields/variants, aliases, conversions, explicit macro arguments,
macro internals, per-item allowances, and host tests with the gate disabled.

## Code facts export

`src/facts_export/` is a pass that reports nothing. It exports what the
compiler knows about the scanned code (items with field types, call edges,
functions used as values, uses of statics, consts, variants and fields,
trait impls, link symbols, atomic orderings, `unsafe` blocks) for the review
code index, which otherwise works from names only.

```bash
make dylint FACTS_OUT=target/facts/kplat-aarch64
```

`FACTS_OUT` runs the normal scan with the export on and packs the result
into that directory:

```text
status.json              running | ready | partial | failed, with a reason
manifest.json            commit, target, toolchain, config digest, crate list
crates/<stem>.jsonl.zst  one zstd-compressed JSON-lines file per crate
```

- **Switch.** The pass writes only while `XKERNEL_FACTS=1`. rustc records
  the variable as an environment dependency, so turning it on or off
  re-checks every workspace crate once; keep it at `1` for every scan that
  shares a target directory (CI does this for the whole dylint stage).
  `FACTS_OUT` sets it for its own scan.
- **Files.** Each checked crate writes `<stem>.facts.jsonl` next to its
  metadata, where `<stem>` is the crate name plus Cargo's extra filename.
  A crate that Cargo finds fresh keeps the file from the run that checked
  it, so a warm scan exports only the crates it re-checks. The packer takes
  the units from Cargo's `compiler-artifact` messages, which list fresh
  units too, so files of other configurations in the same target directory
  are never picked up.
- **Scope.** Workspace members, in the configuration of the scan: the
  selected `.config`, its Cargo features and `--cfg unittest`.
- **Dependencies.** `FACTS_DEPS=y` (`--facts-deps`, or
  `XKERNEL_FACTS_DEPS=1` for every scan of a target directory) also exports
  the crates.io, git and vendored dependencies built for the kernel target,
  with `kind: dep`, their version and features. Cargo then runs `xkmake` as
  its rustc wrapper, which sends those crates through the Dylint driver with
  only the export lint enabled; host crates (build scripts, proc macros) are
  not exported. Turning it on or off rebuilds the dependencies once.
- **Status.** `ready`: every unit has facts. `partial` with reason `build`:
  the scan failed (build error or a lint error) and the units checked
  before it stopped are packed. `partial` with reason `exporter`: the scan
  passed but some units have no valid facts file (`missing` in the
  manifest). `failed`: nothing packed. `running` in a finished CI build
  means the stage was interrupted (timeout or abort).
- **CI.** Both dylint stages pack the facts of their first (normal-graph)
  scan; Jenkins archives them as `artifacts/facts/<platform>/` on every
  outcome. See `docs/jenkins-pipeline.md`.
- **Exporter identity.** `exporter.source_sha256` in the manifest is the
  SHA-256 over the sorted `<path>\0<sha256 of content>\n` lines of this
  directory's `Cargo.toml`, `Cargo.lock`, `rust-toolchain.toml` and `src/`.
  A pull request can change the exporter and so its own facts; a consumer
  that relies on them should accept only digests of a trusted tree (the
  target branch) and treat the facts as review hints, never as a gate.

### Records

The record format version is `SCHEMA` in `src/facts_export/mod.rs`, and the
manifest carries the same value. Within a major version fields are only
added; a change of meaning bumps the version, and readers should reject a
version they do not know. Each line is one JSON object; `t` is its kind.

| `t` | Content |
| --- | --- |
| `crate` | first line: `schema`, `crate`, `crate_id`, `stem`, `package`, `version`, `kind` (`workspace`), `target`, `toolchain`, `rustc`, `crate_types`, `opt_level`, `cfg` (features, `unittest`, `test`, `debug_assertions`) |
| `item` | a definition: `key`, `path`, `kind`, `name`, place, `end_line`, `vis`, `defined_by` (the macro that produced it), `gen` (its name is macro output), `parent`; impls add `self_ty`, `owner`, `trait`, `trait_name`, `negative`; fns add `unsafe`, `generic`, `symbol` (`export_name` / `no_mangle`), `owner`, `trait_name`; closures add `root`; structs and unions add `fields`, enums `variants` (`name`, `key`, `line`, `fields`); statics and consts add `ty`, `ty_adts` |
| `call` | a MIR call or drop edge: `caller`, `caller_root` (enclosing fn of a closure), place, `kind`, `callee_name`, `callee`, `callee_file`, `callee_line`, `callee_owner`, `resolved`, `resolved_owner`, `trait`, `recv`, `generic`, `source` (operator overloading and other non-call syntax), `symbol` (foreign calls), `ty` (dropped type), `orderings` (atomic calls), `in_unsafe`, `inlined`, and for fn-pointer and `Fn*` calls `via_param`, `via_field`, `via_value` or, in a closure, `via_capture` (+ `via_outer`, `via_outer_param`) |
| `ref` | a function or closure used as a value: `caller`, `caller_root`, place, `target`, `closure`, `callee_file`, `callee_line` (where `target` is defined), and where it goes: `passed_to` + `arg` (argument of a call) or `field` (stored into a field) |
| `use` | a use of a static, const, enum variant, field or struct: `caller`, `caller_root`, place, `kind` (`static`, `const`, `variant`, `field`, `struct`), `target`, `access` (`read`, `mut`, `construct`, `match`, `ref`), `target_file`, `target_line` |
| `site` | an `unsafe` block: `kind`, `owner`, place |
| `counts` | last line: totals by kind, `export_us`, `bytes` |

- *Place* fields are `file`, `line`, `col` of the call site in user source
  (the outermost macro invocation for macro output), plus `macro`,
  `inner_macro` and `desugar` (`?`, `for`, `await`, ...) when present.
- *Keys* are the crate name plus the compiler's def path
  (`kspin::{impl#0}::lock`). They are stable within a commit; across commits
  match items by file, line and path.
- *Call kinds*: `static`, `trait_static` (a trait call resolved to an impl,
  in `resolved`), `trait_default`, `trait_generic` (depends on a generic
  parameter of the caller), `trait_unresolved`, `dyn`, `fnptr`, `closure`,
  `foreign`, `intrinsic`, `shim`, `drop` (a user `Drop` impl run by a drop).
  Calls into `core::fmt::rt` (formatting plumbing) are counted in
  `fmt_rt_calls_skipped`, not written. `recv` is the receiver type of a
  `dyn` call or of an `Fn*` trait call on a fn pointer; a call through a
  plain fn pointer has no receiver.
- *Fields* (`fields` of an item) are `name`, `key`, `ty` (the declared type),
  `adts` (keys of the ADTs and `dyn` traits it mentions, outermost first) and
  `line`, so "who holds a `T`" is a key lookup.
- *Uses* come from MIR: place projections (field reads and writes, variant
  downcasts), constants (statics by address, named consts), aggregates
  (constructions) and switches on a discriminant (`match` on a variant).
  Items of `core`, `alloc` and `std` are not recorded. One record per
  target, kind, access and line.
- *Fn values.* For a call through a fn pointer or a generic `Fn*` bound,
  `via_param` is the caller's declared parameter (0-based; in a closure,
  not counting the captured environment) the callee value comes from, `via_field` the field it is read from, `via_value` the function or
  closure itself when the body names it. In a closure, a value read from the
  captured environment gives `via_capture` (the variable's name) and where
  that variable comes from in the enclosing body: `via_outer` +
  `via_outer_param` (a parameter of the enclosing fn), `via_field` (a
  precisely captured field) or `via_value` (a captured function or closure,
  or a `let` initialised with one). The candidates of a `via_param`
  call in `g`, or of a `via_outer` = `g` call with `via_outer_param` as the
  index, are the `ref` records with `passed_to` = `g` and that `arg` (for a
  method, `arg` counts the receiver); those of a `via_field` call are
  the `ref` records with that `field`. Values are followed through locals
  assigned once, references and casts. A body has one `ref` record per
  target, line and destination (`passed_to` + `arg`, or `field`), and a
  plain one only for a line without such a record.
- *Paths* are repository-relative, for dependency crates too (`xkmake` passes
  the repository root in `XKERNEL_FACTS_ROOT`, since Cargo compiles registry
  and git crates from their own directories). Dependencies are written as
  `crates/<name>-<version>/…` (crates.io), `git/<checkout>/<rev>/…` (git) and
  `rust/library/<crate>/…` (standard library).
- *MIR* is taken after drop elaboration and before optimisation, so no call
  is inlined away. For the few bodies whose MIR the compiler already
  optimised during analysis, `const fn` bodies use their const-evaluation MIR
  and others use optimised MIR with inlined calls recovered from the
  inlining scopes (`inlined: true`); the totals count them as
  `mir_ctfe_fn`, `mir_optimized_coroutine` and `mir_optimized_fn`.

## Adding a lint

1. Add `src/<lint>.rs`: declare the lint with `rustc_session::declare_lint!`
   and the pass with `declare_lint_pass!`, then implement the pass the rule
   needs:
   - `LateLintPass`, registered with `register_late_pass`: resolved types,
     paths and macro output. Use it unless the rule is about source syntax.
   - `EarlyLintPass`, registered with `register_pre_expansion_pass`: the
     attributes as written, such as `derive` and `cfg_attr`. It visits every
     module file but not items that macros generate, and it does not expand
     `cfg_attr`.
   - `register_early_pass` runs after expansion, where `derive` and
     `cfg_attr` are already gone.

   Early contexts emit through `LintContext::emit_span_lint`, which takes a
   `rustc_errors::Diagnostic` value instead of a closure.
2. End the module with a `#[cfg(test)]` UI test: an embedded source file and
   the expected compiler output, run through `crate::testing::run_ui` with
   the mock crates the source uses (for example `testing::MOCK_KSPIN`).
3. In `src/lib.rs`, add `mod <lint>;` and register the lint and its pass in
   `register_lints`, after the vendored-crate return so the pass inherits
   that exclusion.
4. Add the rule file `docs/code-guidelines/CG-<nnn>-<descriptive>.md`.

`dylint_testing` runs UI tests under one lock. When a test fails, the lock is
poisoned and every later UI test fails with a `PoisonError`; fix the first
failure, or run one test with `cargo test <lint>`.

## Toolchain

The library links against rustc internals. Bump these together:
`rust-toolchain.toml`, the Makefile `fmt` toolchain, Jenkins
`AUX_RUST_TOOLCHAIN`, and the `dylint_*` / `cargo-dylint` / `dylint-link`
versions (`DYLINT_VERSION`).

CI installs none of the Dylint tooling. The builder image provides
`cargo-dylint`, `dylint-link`, the lint toolchain and its prebuilt driver, and
`scripts/ci/check_dylint_environment.sh`, run at the start of each dylint
stage, fails when they do not match the pins.
Before changing the toolchain or the Dylint version, publish a new image
version (`scripts/ci/docker/`); switch `Jenkinsfile` to it in the same change
and bump its Rustup volume suffix.

The library's own crates in `Cargo.lock` are not vendored. CI fetches them
through the same registry mirror as the kernel crates and keeps them in the
Cargo cache volume.

This standalone host workspace does not inherit the kernel root
`[workspace.lints]`. Validate its compiler-private code and embedded fixtures
with its pinned nightly through `make dylint_test`; kernel lint policy remains
in [the unified guidelines](../../docs/code-guidelines/README.md).
