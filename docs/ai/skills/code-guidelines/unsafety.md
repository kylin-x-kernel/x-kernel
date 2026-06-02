# Unsafety

Use this file for any change that adds,
modifies, or reviews `unsafe` code.

## Mandatory Rules

- every `unsafe` block must have a preceding `// SAFETY:` comment;
- every `unsafe fn` and `unsafe trait` must include a `# Safety` rustdoc section;
- `unsafe impl` must have an explicit safety argument in nearby comments or docs;
- safety arguments must state concrete invariants and who upholds them;
- avoid vague claims like "this is safe by construction"
  without naming the construction and the invariant.
- when a crate or module is expected to remain safe-only,
  do not introduce `unsafe` casually; push unsafe operations behind existing safe layers when possible.

## Audit Surface

- encapsulate unsafe state in the smallest practical module;
- if private fields participate in a safety invariant,
  any code that can mutate them is part of the same audit surface;
- prefer exposing safe wrappers rather than pushing unsafe obligations outward;
- when multiple unsafe paths share one invariant,
  review them together rather than independently.

## Writing Guidance

- explain why the operation is sound under the current invariant;
- mention the guard, lock, state check, ownership rule,
  or lifetime relationship that makes it sound;
- if the safety story depends on ordering or publication,
  say so explicitly.

## When Reviewing

Check specifically for:

- missing `// SAFETY:` comments;
- `unsafe fn` without `# Safety`;
- invariants that are asserted but not actually enforced;
- private mutable state shared across too large a module boundary;
- `unsafe impl` added without a clear thread-safety or ownership argument.
