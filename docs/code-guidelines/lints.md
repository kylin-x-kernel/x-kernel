# Lint 政策与规则覆盖

本目录的统一入口是 [README.md](README.md)。本文件记录 rustc/Clippy 政策与覆盖：
等级在根 `Cargo.toml` 的 `[workspace.lints]`，
参数（阈值、禁用列表、测试豁免）在根 `clippy.toml`。
lint 已强制的规则不再写进 `docs/ai/skills/code-guidelines/`。
`missing_docs` 和 `rustdoc::*` 归 module-docs 轨道（`docs/ai/review/module-docs/`）。

## 等级与检查范围

- MUST 对应 `deny`，SHOULD 对应 `warn`；`make clippy` 带 `-D warnings`，两者都阻塞合入。
- `make clippy` 带 `--cfg unittest` 检查选定平台的依赖图（本地库、proc-macro、host `build.rs`），
  不带 `--all-targets`。因此 `cfg(not(unittest))` 代码、未选中的测试/bench target 从不被 lint。
- `xtask` 是独立 workspace，不继承根 lint；单独运行
  `cargo clippy --manifest-path xtask/Cargo.toml -p xkmake --no-deps`。

## 例外与按 crate 关闭

统一语法、reason 的规则 id、`allow_attributes` 配对、case (a)/(b) 和 fatal-boot
政策只在 [README.md 的 Suppressing a check](README.md#suppressing-a-check) 定义。
各规则的语义例外和已登记的关闭 target 见对应 CG 文件。

## 测试代码

- `#[def_test]` / `#[mod_test]`（`util/macros`）豁免 `unwrap_used`、`expect_used`、`panic`、`too_many_lines`，
  `#[mod_test]` 另豁免 `missing_docs`。测试代码不手写 `allow`/`expect`；内联测试模块用 `#[mod_test]`。
- 文件模块（`mod name;`）不能用属性宏（E0658）：写成 `#[mod_test] mod tests { include!("tests.rs"); }`
  （被 include 的文件不重复声明 cfg 或模块文档），或在辅助函数上逐点写 `<rule-id>: test scenario: ` 例外。
- host 侧 `#[test]` 由 `clippy.toml` 的 `allow-*-in-tests` 覆盖。

## 新增 lint 与工具链升级

- 一条 lint 一个提交：启用（含 `clippy.toml` 参数）、处理全部存量、在下表登记、从 skill 删除被覆盖的原文。
- 验证：`Jenkinsfile` 中每个 `make clippy` 配置；aarch64 `qemu_defconfig` 的 `make clippy UNITTEST=y`；
  命中随 cfg/feature 变化的 lint 另分别开、关可选 feature 跑。
  loongarch64 目前有无关的既有失败，比较启用前后的错误集合即可。
- 修改 `rust-toolchain.toml` 时跑同一矩阵，并确认本文件中的 lint 名和 `clippy.toml` 键仍存在。

## 与 AI 审查的分工

- full 的机械部分不重复审查；partial 的判断、例外理由见对应 CG 文件。
- case (b) 关闭的 crate，新增代码仍按该行规则审查；case (a) 按不适用范围处理。
- 统一规则入口见 README.md；尚未迁移的规则仍以 skill 为准。AI 只审带 ai check 的 CG 文件。

## 已启用 lint

规则原文、人工判断与已登记 opt-out 见 CG 文件；此表只记录后端政策和规则指针。
MUST/SHOULD 是规则要求；下列等级以 Cargo.toml 为准，make clippy 将 warn 提升为错误。

| lint | 等级与参数 | 规则文件 | 覆盖 |
|---|---|---|---|
| `clippy::undocumented_unsafe_blocks` | deny | [CG-016-safety-comment-substance](CG-016-safety-comment-substance.md) | partial |
| `clippy::disallowed_methods` | warn；disallowed-methods 禁 core::mem::zeroed | [CG-018-uninitialized-storage-model](CG-018-uninitialized-storage-model.md) | partial |
| `clippy::panic` | warn；allow-panic-in-tests | [CG-021-no-panic-on-reachable-failure](CG-021-no-panic-on-reachable-failure.md) | partial |
| `clippy::unwrap_used` | warn；allow-unwrap-in-tests | [CG-021-no-panic-on-reachable-failure](CG-021-no-panic-on-reachable-failure.md) | partial |
| `clippy::expect_used` | warn；allow-expect-in-tests | [CG-021-no-panic-on-reachable-failure](CG-021-no-panic-on-reachable-failure.md) | partial |
| `clippy::todo` | deny | [CG-023-unfinished-implementations](CG-023-unfinished-implementations.md) | partial |
| `clippy::unimplemented` | deny | [CG-023-unfinished-implementations](CG-023-unfinished-implementations.md) | partial |
| `clippy::allow_attributes` | deny | [CG-024-lint-exceptions](CG-024-lint-exceptions.md) | partial |
| `clippy::allow_attributes_without_reason` | deny | [CG-024-lint-exceptions](CG-024-lint-exceptions.md) | partial |
| `clippy::multiple_unsafe_ops_per_block` | deny | [CG-025-minimal-unsafe-scope](CG-025-minimal-unsafe-scope.md) | partial |
| `unsafe_op_in_unsafe_fn` | deny | [CG-025-minimal-unsafe-scope](CG-025-minimal-unsafe-scope.md) | partial |
| `clippy::missing_safety_doc` | deny；check-private-items = true | [CG-026-unsafe-api-safety-docs](CG-026-unsafe-api-safety-docs.md) | full |
| `unreachable_pub` | warn | [CG-027-narrow-visibility](CG-027-narrow-visibility.md) | partial |
| `clippy::too_many_lines` | warn；阈值 100 | [CG-028-focused-functions](CG-028-focused-functions.md) | partial |
| `clippy::excessive_nesting` | warn；阈值 6 | [CG-029-shallow-control-flow](CG-029-shallow-control-flow.md) | partial |
| `clippy::fn_params_excessive_bools` | warn；max-fn-params-bools = 3 | [CG-030-explicit-boolean-parameters](CG-030-explicit-boolean-parameters.md) | partial |
| `clippy::linkedlist` | warn | [CG-031-intrusive-linked-lists](CG-031-intrusive-linked-lists.md) | partial |
| `clippy::upper_case_acronyms` | warn；upper-case-acronyms-aggressive = true | [CG-032-camel-case-types](CG-032-camel-case-types.md) | full |
| `clippy::unused_result_ok` | warn | [CG-033-explicit-result-discard](CG-033-explicit-result-discard.md) | full |
| `clippy::used_underscore_binding` | warn | [CG-034-used-binding-names](CG-034-used-binding-names.md) | full |
| `unexpected_cfgs` | warn；check-cfg = cfg(unittest) | [CG-035-declared-cfgs](CG-035-declared-cfgs.md) | full |

`avoid-breaking-exported-api = false` 让上表及其他默认 Clippy lint 也检查 pub 项。

## 待迁入表

当前没有待启用的 rustc/Clippy 候选。早期 part 1 报告中的 todo、panic、
too_many_lines、excessive_nesting、fn_params_excessive_bools 已在本分支启用；
以当前 Cargo.toml 和 clippy.toml 为准，不以旧报告恢复 pending 状态。
Dylint 的待迁入表在 stack 落地后的 pending.md。

| lint | 目标等级 | 待定政策 / 阻塞项 |
|---|---|---|

## 开放问题

- `TODO(cg)` 逐点原因仍登记失败接口、生命周期或拆分债务；启用 lint 不代表债务已解决。
  具体判断见对应 CG 文件，不把开放问题当作额外豁免。
- feature/platform 依赖的命中须按新增 lint 流程分别开、关配置验证；
  单个平台的可达性不证明所有配置都不使用 API。

## 辅助性 lint

默认开启的 lint 不进入 workspace 显式注册表；只查原文的一个侧面时，原文保留在 skill。
唯一承载一条规则的默认 lint 则有 CG 文件：

| lint | 默认等级 | 覆盖 / 规则 |
|---|---|---|
| clippy::question_mark | warn | partial；[CG-036-ordinary-error-propagation](CG-036-ordinary-error-propagation.md) |
| non_camel_case_types | warn | CamelCase 语法，辅助 CG-032 |
| invalid_value | warn | 已知无效位模式，辅助 CG-018 |
| clippy::uninit_assumed_init | deny | 直接 uninit().assume_init()，辅助 CG-018 |

其他辅助 lint：

- `static_mut_refs`（默认）：只拦对 `static mut` 取引用，不拦声明。
- `improper_ctypes` / `improper_ctypes_definitions`（默认）：只查 FFI 签名类型。
- `clippy::wrong_self_convention`（默认）：只查 `as_`/`to_`/`into_` 前缀与 `self` 形式。
- `clippy::too_many_arguments`（默认）：只数参数个数。
- `unsafe_code`：由 safe-only crate 自行在根部 `#![forbid(unsafe_code)]`，不统一配置。

## 不采用的 lint

`clippy::let_underscore_must_use`：有意丢弃就写 `let _ = expr;`，原因不明显时注释说明；
`.ok();` 已由 `unused_result_ok` 禁止。Rust-for-Linux 两者都未启用。
