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

## Public Model Boundaries

- public APIs should expose stable semantic objects and capability-focused helpers,
  not internal aggregate state objects;
- prefer returning the specific capability the caller needs, such as resources,
  filesystem context, or address space, instead of a catch-all internal runtime object;
- do not expose transitional compatibility aliases, construction-phase objects,
  or bridge types as part of the long-term public model;
- if a type exists mainly to wire together internal subsystems,
  it should usually stay crate-private even when heavily used internally;
- before making a type or function public, ask whether callers are depending on
  a real semantic contract or on the crate's current implementation layout.

## When Reviewing

Check specifically for:

- APIs that force callers to remember hidden invariants;
- boolean flags that should be split into clearer entry points;
- public fields or modules that expose implementation details;
- public contracts that leak representation or internal policy.
- public helpers that return broad internal state objects when a narrower capability
  helper would keep the boundary cleaner;
- compatibility or migration shims that accidentally became part of the public model.
