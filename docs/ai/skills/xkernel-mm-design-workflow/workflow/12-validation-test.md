# Phase 12: Validation Test

Owner:

- Validation Tester

Create:

- `12-validation-report.md`

Rules:

- Use the project build workflow rather than bare `cargo check`.
- Map every test to a design contract from `09-implementation-scope.md`.
- Runtime or syscall behavior requires runtime or guest validation when
  feasible.
- Record commands that could not run and why.
- Do not mark implementation ready if critical review findings remain.
