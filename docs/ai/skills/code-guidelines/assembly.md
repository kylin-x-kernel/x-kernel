# Assembly

Use this file when the change touches `.S`,
`global_asm!`, labels, or Rust-callable assembly entry points.

## Sections And Width

- use the correct section directive for built-in vs custom sections;
- keep code-width directives adjacent to the section definition when applicable;
- visually separate section declarations from the code that follows.

## Functions And Labels

- place function attributes directly before the function label;
- add `.type` and `.size` for Rust-callable assembly functions when meaningful;
- use unique label prefixes to avoid collisions inside a crate-wide translation unit;
- prefer `.balign` over architecture-ambiguous `.align`.

## When Reviewing

Check specifically for:

- section metadata that does not match the intended use;
- missing type/size metadata on Rust-callable functions;
- generic labels likely to collide across `global_asm!` units;
- alignment directives whose meaning changes by architecture.
