# API Design

Use this file when the change introduces or reshapes
functions, methods, types, traits, modules, or public contracts.

## Functions And Methods

- keep functions small and focused;
- minimize unnecessary nesting;
- avoid boolean arguments when the call site becomes unclear;
- use explaining variables or block expressions
  when they make control flow easier to read;
- prefer checked or saturating arithmetic
  when overflow is a realistic correctness concern.

## Types And Invariants

- prefer types that encode invariants explicitly;
- use enums instead of trait objects for closed sets;
- encapsulate internal fields behind methods when that preserves future flexibility;
- expose the narrowest contract that callers actually need.
- hide implementation details from public APIs and their docs.

## When Reviewing

Check specifically for:

- APIs that force callers to remember hidden invariants;
- boolean flags that should be split into clearer entry points;
- public fields or modules that expose implementation details;
- public contracts that leak representation or internal policy.
