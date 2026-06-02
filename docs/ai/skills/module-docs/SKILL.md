# Module Docs

Use this skill when an AI agent needs to create,
update, or review module documentation for an X-Kernel crate.

This skill is the canonical workflow for generating module docs.
It covers the crate-local Markdown documents
and the rustdoc content that must stay in sync with code.

## Goal

Produce documentation that is:

- colocated with the crate being documented;
- derived from the current code and APIs,
  not from generic template filler;
- aligned with the shared module-doc skill and existing module-doc style;
- reviewable together with the code change that introduced the behavior.

## Output Layout

Each documented crate should keep its docs under the crate directory:

```text
<crate>/
├── src/
├── Cargo.toml
└── docs/
    ├── design.md
    └── security.md
```

Example:

```text
util/klazy/
├── src/
├── Cargo.toml
└── docs/
    ├── design.md
    └── security.md
```

## Source Material

When generating module docs,
use these sources in priority order:

1. the current crate source code;
2. existing crate docs under `<crate>/docs/`;
3. repository-local examples such as `util/klazy/docs/`;
4. this shared skill as the canonical structure and review workflow.

Do not write module docs by copying generic structure verbatim
without first extracting the actual design from the code.

## Document Split

### `docs/design.md`

Use `design.md` for:

- module purpose and scope;
- architecture and component relationships;
- execution-context and calling constraints;
- state machines;
- algorithms and critical flows;
- concurrency model;
- design decisions and tradeoffs;
- resource lifecycle and drop behavior.

### `docs/security.md`

Use `security.md` for:

- trust boundaries;
- external attack surfaces and input boundaries;
- unsafe code inventory;
- memory-safety invariants;
- thread-safety conditions;
- threat analysis;
- FMEA;
- failure handling;
- privacy analysis;
- known limitations;
- audit checklist.

### rustdoc in `src/`

Keep API-level documentation in rustdoc instead of duplicating it
in `docs/design.md` or `docs/security.md`.

Rustdoc should carry:

- public API semantics;
- parameter and return-value explanation;
- error behavior;
- panic behavior;
- `# Safety` requirements for unsafe APIs;
- examples for core public APIs where practical.

## Required Rustdoc Coverage

The following items should be documented in code comments
rather than repeated in crate-local Markdown:

- crate-level public API overview;
- public functions and methods;
- public types, traits, and modules;
- unsafe usage contracts;
- panic and error behavior.

For important public APIs,
rustdoc is expected to include:

- a one-line purpose statement;
- `# Arguments` when arguments need explanation;
- `# Returns` when the return contract is non-trivial;
- `# Errors` when returning `Result`;
- `# Safety` for unsafe functions;
- `# Panics` when panic is possible;
- `# Example` for core APIs when practical.

## `design.md` Structure

Use this structure unless a section is truly inapplicable:

```text
# <module> — 设计文档
## 定位
## 背景
## 范围
## 架构
## 调用约束 / 执行上下文
## 状态机
## 算法流程
## 并发模型
## 设计决策
## Drop / 资源释放
```

Section guidance:

- `定位`:
  state the module's role in X-Kernel
  and which subsystems depend on it.
- `背景`:
  explain why the module exists
  and what environment or constraint it addresses.
- `范围`:
  list the relevant source files for the documented design.
- `架构`:
  include an ASCII diagram when it helps.
- `调用约束 / 执行上下文`:
  state where the module may run
  and what execution assumptions callers must satisfy.
- `状态机`:
  keep only when the module truly has state transitions.
- `算法流程`:
  describe core flows step by step.
- `并发模型`:
  explain locks, atomics, reentrancy, and hot-path tradeoffs.
- `设计决策`:
  record important choices and rejected alternatives.
- `Drop / 资源释放`:
  explain resource lifetime and cleanup behavior
  when it is not obvious from the types alone.

If a section does not apply,
delete it instead of filling it with empty text.

For kernel crates,
the `调用约束 / 执行上下文` section should explicitly consider:

- whether the API may run in interrupt context;
- whether it is valid during early boot;
- whether it requires a current process thread
  or only an execution path;
- whether it may sleep or block;
- whether it is reentrant;
- whether it depends on CPU-local state,
  platform initialization,
  memory mappings,
  or scheduler availability.

## `security.md` Structure

Use this structure unless a section is truly inapplicable:

```text
# <module> — 安全与可靠性分析
## 信任模型
## 外部边界 / 攻击面
## unsafe 代码清单
## 内存安全不变量
## 线程安全
## 威胁分析
## 故障模式与影响分析（FMEA）
## 故障管理
## 隐私分析
## 已知限制
## 审计清单
```

Section guidance:

- `信任模型`:
  show the boundary between callers and the module.
- `外部边界 / 攻击面`:
  identify what untrusted or partially trusted inputs
  can reach the module.
- `unsafe 代码清单`:
  enumerate unsafe functions or blocks with real invariants,
  safety reasoning, and guarded call paths.
- `内存安全不变量`:
  state the conditions that must always hold for soundness.
- `线程安全`:
  describe `Send`/`Sync` conditions and concurrency assumptions.
- `故障管理`:
  explain `Result`, panic, poisoning, retry, or recovery behavior.
- `隐私分析`:
  explicitly say when the module does not process user data.
- `已知限制`:
  list real technical constraints,
  not aspirational future ideas.
- `审计清单`:
  convert module-specific pitfalls into reviewer checks.

If the module has no unsafe code,
remove or greatly shorten the unsafe-specific sections
instead of fabricating content.

For kernel crates,
the `外部边界 / 攻击面` section should explicitly check
whether the module interacts with:

- user memory or user-provided pointers;
- MMIO or PIO registers;
- DMA buffers or device-owned memory;
- FFI, inline assembly, or architecture-specific raw interfaces;
- bootloader, firmware, device tree, ACPI, or other boot-time metadata;
- device input, interrupts, or hardware status;
- file-system, network, IPC, or other externally sourced data.

If a boundary is relevant,
record it here as a boundary definition.
Do not rely on this section alone as the full threat analysis.

Section split:

- `外部边界 / 攻击面`:
  define the risk sources and trust boundaries.
- `威胁分析`:
  enumerate concrete threats derived from those boundaries,
  including trigger conditions, impact, and mitigations.
- `故障模式与影响分析（FMEA）`:
  analyze operational failure modes and their local/system effects.

`威胁分析` should reference the identified boundaries directly
instead of discussing only Rust-level `unsafe`.

For the `应对措施` field in threat analysis:

- first describe the controls already implemented in the code or design;
- if current controls are incomplete, add the recommended hardening measures;
- when a risk is intentionally accepted or only partially mitigated,
  state the residual risk explicitly;
- avoid vague phrases such as "加强校验" without naming the real control.

## Unsafe Traceability

Unsafe documentation must be traceable back to code.

For each important unsafe path,
record at least:

- function or method name;
- source file path;
- line number when practical;
- invariant being relied on;
- why the code is safe under that invariant;
- which safe entry points or guards enforce the invariant.

If multiple unsafe blocks participate in one safety story,
group them only when the shared invariant is explicit.

## Table Formats

Threat analysis table:

```markdown
| 编号 | 威胁描述 | 影响等级 | 触发条件 | 应对措施 |
|------|----------|----------|----------|----------|
| T-01 | ...      | 高/中/低 | ...      | ...      |
```

Severity guide:

- `高`: UB, memory corruption, privilege escalation
- `中`: panic, service unavailability, inconsistent state
- `低`: performance degradation, lost logs, degraded functionality

FMEA table:

```markdown
| 编号 | 故障模式 | 故障原因 | 局部影响 | 系统影响 | 严重度 | 应对措施 |
|------|----------|----------|----------|----------|--------|----------|
| F-01 | ...      | ...      | ...      | ...      | 1-4    | ...      |
```

Severity guide:

- `1`: fatal
- `2`: serious
- `3`: moderate
- `4`: minor

## Generation Workflow

When generating module docs for an existing crate:

1. Read the crate entry points in `src/lib.rs` or `src/mod.rs`.
2. Identify the key exported types, state, and cross-file structure.
3. Inspect the main implementation files,
   especially unsafe, concurrent, or stateful paths.
4. Extract execution-context constraints from the code paths and callers.
5. Draft `docs/design.md` from the actual architecture.
6. Identify external boundaries, unsafe paths,
   invariants, and failure modes.
7. Draft `docs/security.md`
   from those real boundaries and constraints.
8. Update rustdoc for the public APIs touched by the documented behavior.
9. Re-read the docs against the code
   to remove any unsupported claims or template leftovers.

When generating docs for a new crate:

1. Create `<crate>/docs/` if missing.
2. Add `design.md` first.
3. Add `security.md` once safety boundaries and failure behavior are clear.
4. Keep rustdoc in lockstep with the initial public API.

## Doc Update Triggers

When a code change touches an already documented crate,
explicitly check whether the docs need to change.

Documentation updates are usually required when the patch changes:

- public API behavior, contracts, or error semantics;
- crate architecture, file layout, or the role of major types;
- execution-context or caller obligations;
- state machines, lifecycle, cleanup, or drop behavior;
- `unsafe` paths, safety invariants, or thread-safety conditions;
- external input boundaries, trust assumptions, or threat analysis;
- failure modes, recovery behavior, or known limitations.

Update targets:

- `docs/design.md` for architecture, execution context,
  state, flow, concurrency, and lifecycle changes;
- `docs/security.md` for trust boundaries, unsafe inventory,
  invariants, threats, FMEA, or failure-management changes;
- rustdoc for public API contract changes.

## Writing Rules

- Write from code facts, not from guessed architecture.
- Prefer concrete names from the module over generic placeholders.
- Replace every template placeholder before finishing.
- Use ASCII diagrams when they materially improve comprehension.
- Explain why a design exists, not just what files exist.
- State caller obligations and execution assumptions explicitly.
- Treat hardware, firmware, user memory,
  and external input as first-class documentation inputs.
- Keep the Markdown focused on module-level design;
  do not duplicate line-by-line API docs from rustdoc.
- If a section does not apply, delete it.
- If a template statement is no longer true for the code, rewrite it.

## Review Checklist

Before considering the docs complete, verify:

- `docs/design.md` matches the current file layout and major types.
- execution-context constraints match the actual callers and runtime model.
- `docs/security.md` matches the current unsafe and failure behavior.
- external boundaries and input sources are identified where relevant.
- rustdoc covers the public APIs introduced or changed by the module.
- no placeholder text such as `[模块名]` remains.
- diagrams, tables, and section names use the crate's real terminology.
- design claims are consistent with current code,
  atomics, locks, state transitions, and drop behavior.
- important unsafe paths are traceable from the doc back to code locations.

## When To Say "无"

For sections that are genuinely not applicable:

- use brief explicit statements such as `无`
  only when the section is required for review context;
- otherwise delete the section entirely.

Examples:

- no state machine;
- no user-data handling;
- no redundant design;
- no overload-control design;
- no human-factor design.

## Related Repository Files

- `util/klazy/docs/design.md`
- `util/klazy/docs/security.md`
