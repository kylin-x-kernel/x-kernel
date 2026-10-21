# Code Statistics

Run from any directory with Python 3.10+ and tokei with JSON support (tested
with tokei 14.0.0). No kernel configuration, Cargo build or network is needed.

```bash
python3 scripts/code_stats.py
python3 scripts/code_stats.py --format json > /tmp/xkernel-code-stats.json
python3 scripts/code_stats.py --root /path/to/another/x-kernel
```

`--tokei /path/to/tokei` selects the executable. Text output shows category and
language subtotals: files, code, comments, blank lines and total lines. JSON
also includes every counted file, its category/language, uncounted paths and
reasons, the Git revision, tracked-worktree dirty state, and tokei version.
Output is sorted for comparison; compare clean checkouts using the same tokei
version and classification rules.

JSON schema version 2 records the source-language gate, ordered `path_rules`
(first match wins), and fallback category in `rules`. Classification and this
snapshot share one rule table. Match fields are ORed: `roots` matches the first
repository-relative component, `directories` matches any directory component,
`files` matches the entire relative path, and `filenames` matches the basename.
This includes the `build.rs`, `linker.lds.S`, and `fuzz/` special cases. Compare
the entire `rules` object, including list order; version 1 snapshots were
incomplete and must not be treated as an equivalent classification contract.

## Counting Scope

The script supplies **Git-tracked regular files** to tokei in bounded batches,
not repository directories. This prevents untracked build outputs, `disk.img`,
downloaded code, Cargo registry dependencies and generated reports from being
counted. Staged additions are included; untracked new files are not. Contents
come from the working tree, including unstaged edits, not the Git index or HEAD.
The printed commit is provenance, not a claim that local contents equal it.

Explicitly tracked generated files and tracked files under ignored directories
remain included. Symlinks, submodules, missing files and files tokei cannot
recognize are listed under `skipped`; submodules are not recursively expanded.
An unresolved Git index conflict, missing tool, failed invocation, invalid JSON,
or tokei's inaccurate-count flag fails the command. Empty input never falls back
to scanning the current directory.

All line counts come from tokei. Embedded languages are recursively included
once and attributed to their **containing file**. In particular, Rust doc
comments and embedded examples follow tokei's counting semantics; the script
does not claim that every `code` line is compiled executable code. Markdown
examples remain in the non-source category, not kernel or test source totals.

## Classification

The readable whitelist and directory rules live at the top of `code_stats.py`.
Directory matching uses path components anchored at the Git repository root,
not absolute-path substrings or a first occurrence of `src`.

Rules apply in this order; each counted file belongs to exactly one category:

| Category | Rule |
| --- | --- |
| `non_source` | Language not in `SOURCE_LANGUAGES`: documents, TOML/JSON/YAML, device-tree/linker configuration, or other languages not yet admitted |
| `tests` | Source under any `ut-tests/`, `tests/`, `abi-tests/`, `benches/` component, the root `fuzz/` tree, or the explicit `TEST_FILES` whitelist of legacy TEE test scripts |
| `tools` | Source under `scripts/`, `tools/`, `xtask/`, `.githooks/`, `.github/`, `.ci/`; root `Makefile`/`Jenkinsfile`; any `build.rs` |
| `third_party` | Remaining source under `third_party/` |
| `applications` | Remaining source under `uapps/` or `tee_apps/`, plus the user-space bootstrap `entry/src/init.sh` |
| `kernel` | Remaining source under the explicit kernel subsystem roots, plus root `linker.lds.S` |
| `other_source` | Recognized source outside those rules, requiring classification review |

There is deliberately no blanket category for `docs/`: experimental probes
(`docs/experiments/*/probe.c`), examples and review helpers (such as
`docs/ai/review/module-docs/export_checkpoints.py`) remain `other_source` unless
a more specific rule matches. They are included in `ALL SOURCE`, not omitted.
Markdown itself remains `non_source`; a source file under `docs/**/tests/`
still follows the test-directory rule. New fallback paths should be reviewed
individually rather than treating all documentation-adjacent source as tools
or tests. The text report points to JSON `rules` instead of duplicating the
test-directory list.

The real guest test directory is **`ut-tests`**, not `uta-tests`. Test helpers,
fixtures recognized as source, host unit/integration tests, benchmarks and fuzz
sources all count as test code. The rule does not search Rust ASTs or split
inline tests out of a production file; keep the separate layout checks enabled.
Tool tests and third-party tests count under `tests`, not twice under their
parent category. Directory ownership is not a license or original-authorship
classification. Small test hooks still in production source count as kernel.

`ALL SOURCE = kernel + tests + tools + third_party + applications + other_source`.
`ALL RECOGNIZED = ALL SOURCE + non_source`. Language subtotals use the same
records, so every metric reconciles. Neither total includes `skipped` files.
Review the JSON's `skipped` list before presenting a number as exhaustive:
for example, tokei 14 does not recognize Kconfig files or the repository's
version-suffixed `Dockerfile_v3.*` names. They are reported, not guessed to be Rust
or silently included in kernel LOC. Adding a language to the whitelist changes
the source/non-source split, not tokei's recognition of filenames.

## Regression Tests

```bash
python3 -m unittest discover -s scripts/tests -p 'test_code_stats.py' -v
```

The suite tests classification precedence, component boundaries, embedded
languages, empty input, batching, unmerged index stages, invalid repository
roots, rule snapshot consistency, failures, and total reconciliation. Its real
Git/tokei fixture includes paths with spaces, a checkout path containing `src`,
hidden/ignored tracked files, untracked files, deletions and symlinks. The real
CLI is also tested from outside the repository, including JSON/text agreement,
revision provenance and clean/dirty working-tree states. Batching verifies that
every input is counted exactly once across batch boundaries. The real
integration test is skipped if Git or tokei is unavailable; a full validation
must install both and report no skips. This reporting tool is manual and does
not change kernel build or CI gates.
