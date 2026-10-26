# Naming And Structure

Apply these rules whenever code introduces or renames
types, functions, fields, locals, modules, or files.

## Names

- use descriptive names;
- use accurate names that reflect actual semantics and cost;
- prefer familiar Rust and Linux naming conventions
  over project-local synonyms;
- prefer full words over ambiguous abbreviations;
- avoid names that hide collection scans or side effects
  behind accessor-sounding verbs.
- end closure or function-pointer locals with `_fn`
  when that makes callability explicit.

Examples:

- use `read_command()`, not `command()`
- use `collect_all()`, not `get_all()` when the path is O(n)
- use `task_fn` for a closure local, not `task`

## File And Module Structure

- keep one major concept per file when practical;
- organize files and impl blocks for top-down reading;
- place public entry points before private helpers where possible;
- split files when multiple distinct concepts start competing for attention;
- prefer existing workspace abstractions and dependencies
  over ad hoc local reinvention.

## When Reviewing

Check specifically for:

- misleading names that hide cost, units, or side effects;
- modules that are too broad to audit comfortably;
