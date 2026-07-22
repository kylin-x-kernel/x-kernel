# Git And Testing

Use this file when the task includes test updates,
bug-fix validation, or commit/PR structuring guidance.

## Commits

- one logical change per commit;
- separate refactoring from feature work;
- keep pull requests focused on a single topic;
- write imperative, descriptive commit subjects.

## Cargo.lock

The workspace root `Cargo.lock` is tracked for reproducible builds.
It is machine-generated and must stay consistent with `Cargo.toml`;
never hand-edit it — it records exact versions and content checksums,
and a manual merge can leave it inconsistent or unparseable.

The kernel builds for four targets
(`x86_64-unknown-none`, `aarch64-unknown-none-softfloat`,
`riscv64gc-unknown-none-elf`, `loongarch64-unknown-none-softfloat`),
so the committed lockfile must cover all of them, not just the host.

### Resolving `Cargo.lock` conflicts during rebase

A conflict means both branches changed dependency versions.
Because the file is generated, resolve it by regenerating,
not by editing conflict markers by hand:

```bash
rm -f Cargo.lock                                   # drop the conflicted file
cargo generate-lockfile                            # regenerate from the merged Cargo.toml
cargo metadata --locked --format-version=1 >/dev/null   # verify it is a valid locked resolution
git add Cargo.lock
git rebase --continue
```

`cargo generate-lockfile` resolves the graph across all four kernel
targets, so the regenerated lockfile stays complete for every platform.
`cargo fetch` produces the same lockfile but also downloads crate
sources, so prefer `generate-lockfile` at rebase time.
`cargo metadata --locked` must succeed before continuing — it is the
same check `scripts/check_deps.py` runs, and a failure means the
lockfile is out of sync with `Cargo.toml`.

For a single dependency bump rather than a full conflict,
update just that crate and let Cargo reconcile the rest:

```bash
cargo update -p <crate-name>
```

## Tests

- add a regression test for every real bug fix when practical;
- test user-visible behavior rather than implementation details;
- use assertion helpers instead of manual log inspection;
- clean up resources after each test;
- name tests after the behavior or specification concept being verified.

## When Reviewing

Check specifically for:

- fixes that changed behavior without adding regression coverage;
- tests over-coupled to internal implementation names;
- resource leaks across test cases;
- feature work mixed with unrelated cleanup in the same commit series.
