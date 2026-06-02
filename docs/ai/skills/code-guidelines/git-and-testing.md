# Git And Testing

Use this file when the task includes test updates,
bug-fix validation, or commit/PR structuring guidance.

## Commits

- one logical change per commit;
- separate refactoring from feature work;
- keep pull requests focused on a single topic;
- write imperative, descriptive commit subjects.

## Tests

- add a regression test for every real bug fix when practical;
- test user-visible behavior rather than implementation details;
- use assertion helpers instead of manual log inspection;
- clean up resources after each test;
- name tests after the behavior or specification concept being verified.

## When Reviewing

Check specifically for:

- fixes that changed behavior without adding regression coverage;
- tests over-coupled to internal implementation names;
- resource leaks across test cases;
- feature work mixed with unrelated cleanup in the same commit series.
