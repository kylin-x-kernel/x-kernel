# API Design

Use this file when the change introduces or reshapes
functions, methods, types, traits, modules, or public contracts.

## Functions And Methods

- use explaining variables or block expressions
  when they make control flow easier to read;
- prefer checked or saturating arithmetic
  when overflow is a realistic correctness concern.

## Types And Invariants

- prefer types that encode invariants explicitly;
- use enums instead of trait objects for closed sets;
- encapsulate internal fields behind methods when that preserves future flexibility;

## Public Model Boundaries

- before making a type or function public, ask whether callers are depending on
  a real semantic contract or on the crate's current implementation layout.

## When Reviewing

Check specifically for:

- APIs that force callers to remember hidden invariants;
