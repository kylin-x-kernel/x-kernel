# Naming And Structure

Apply these rules whenever code introduces or renames
types, functions, fields, locals, modules, or files.

## Names

- use descriptive names;
- use accurate names that reflect actual semantics and cost;
- prefer familiar Rust and Linux naming conventions
  over project-local synonyms;
- encode units in names when the type does not;
- keep boolean names assertion-like
  (`is_*`, `has_*`, `can_*`, `should_*`, `needs_*`);
- prefer full words over ambiguous abbreviations;
- avoid names that hide collection scans or side effects
  behind accessor-sounding verbs.
- follow Rust CamelCase and acronym capitalization for type names;
- end closure or function-pointer locals with `_fn`
  when that makes callability explicit.

Examples:

- use `timeout_ns`, not `timeout`
- use `size_pages`, not `size`
- use `read_command()`, not `command()`
- use `collect_all()`, not `get_all()` when the path is O(n)
- use `IoMemoryArea`, not `IOMemoryArea`
- use `task_fn` for a closure local, not `task`

## File And Module Structure

- keep one major concept per file when practical;
- organize files and impl blocks for top-down reading;
- place public entry points before private helpers where possible;
- split files when multiple distinct concepts start competing for attention;
- default to the narrowest visibility that works;
- prefer existing workspace abstractions and dependencies
  over ad hoc local reinvention.

## When Reviewing

Check specifically for:

- misleading names that hide cost, units, or side effects;
- negated boolean names;
- modules that are too broad to audit comfortably;
- helpers exposed more widely than required.
