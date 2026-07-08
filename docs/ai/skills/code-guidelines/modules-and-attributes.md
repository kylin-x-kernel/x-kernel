# Modules And Attributes

Use this file when the change touches visibility,
imports, crate dependencies, macros, lints, or attributes.

## Visibility And Imports

- default to the narrowest visibility that works;
- keep module boundaries aligned with ownership and audit surface;
- define crate boundaries around cohesive ownership domains:
  tightly related data, state, invariants, lifecycle rules, and execution-context
  assumptions that naturally need to evolve together;
- prefer keeping code as a `mod` when it is an implementation detail owned by one crate and
  is not a shared ownership boundary for multiple peer crates;
- split a `mod` into an independent crate when the same owned data/state boundary,
  state machine, or invariant-bearing component is a 1-to-N dependency reused by
  multiple peer crates;
- avoid catch-all crates that aggregate unrelated resources only because migration or wiring
  is temporarily convenient;
- keep internal representation objects, staging types, registry plumbing,
  and bridge helpers at `pub(crate)` unless they are intentionally part of the crate's
  external semantic model;
- a crate should publicly expose its natural domain objects and stable capabilities,
  not the internal structures used to assemble or cache them;
- prefer importing a parent module and qualifying free-function calls
  rather than importing free functions directly by name;
- use workspace dependencies instead of ad hoc per-crate version drift.

## Attributes

- keep attributes ordered predictably;
- place `#[derive(...)]` after other outer attributes;
- keep derived traits ordered consistently;
- suppress lints at the narrowest practical scope;
- use `#[expect(dead_code)]` only when the dead code is simple,
  clearly intentional, and likely to become used.

## Macros

- prefer functions and generics over macros when ordinary Rust can express the idea;
- reach for macros only when syntax shaping,
  compile-time generation, or type-system limits justify it;
- avoid macro use that makes control flow or ownership harder to audit.

## When Reviewing

Check specifically for:

- over-wide visibility;
- crates whose contents do not share a coherent ownership, state, and invariant model;
- implementation-only modules promoted to crates without a real cross-crate reuse boundary;
- shared state/data components left inside one crate even though multiple peer crates depend
  on them;
- internal structs or free functions that were made public only for convenience,
  testing, or migration wiring;
- direct free-function imports that remove call-site context;
- dependency declarations that bypass workspace conventions;
- broad lint suppression;
- attributes or derives ordered inconsistently with the project convention;
- macros used where a normal function would be clearer.
