# Modules And Attributes

Use this file when the change touches visibility,
imports, crate dependencies, macros, lints, or attributes.

## Visibility And Imports

- default to the narrowest visibility that works;
- keep module boundaries aligned with ownership and audit surface;
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
- direct free-function imports that remove call-site context;
- dependency declarations that bypass workspace conventions;
- broad lint suppression;
- attributes or derives ordered inconsistently with the project convention;
- macros used where a normal function would be clearer.
