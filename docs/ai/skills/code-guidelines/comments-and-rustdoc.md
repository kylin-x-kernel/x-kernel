# Comments And Rustdoc

Use this file when the change touches comments,
doc comments, module docs, or design rationale.

## Comments

- comments should explain why, not restate what the code does;
- if a comment is compensating for confusing code,
  simplify the code first;
- document non-obvious design decisions and tradeoffs;
- cite external specs, ABIs, man pages, or algorithm sources
  when behavior depends on them;
- prefer semantic line breaks in prose-heavy comments and Markdown.

## Rustdoc

- document public semantics in rustdoc,
  not in ad hoc inline comments;
- add module-level documentation for major components;
- keep doc comments focused on contract and behavior,
  not implementation detail leakage;
- use backticks around identifiers in prose when it improves clarity;
- summary lines should read like proper API summaries,
  not incomplete fragments or implementation notes.
- end full-sentence comments with punctuation.

## Interaction With Module Docs

- API-level contracts belong in rustdoc;
- crate-level design/security analysis belongs in
  `docs/design.md` and `docs/security.md`
  under the module-docs skill;
- do not duplicate the same contract in three places.

## When Reviewing

Check specifically for:

- comments that just paraphrase the code;
- missing rationale on non-obvious choices;
- stale comments after behavior changes;
- public APIs without adequate rustdoc on behavior,
  errors, panics, or safety contracts.
