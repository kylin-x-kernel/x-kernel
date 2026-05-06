# 编码规范

本章节描述了 Asterinas 项目的编码与协作约定。
这些规范旨在确保代码的
清晰、一致、可维护、正确以及高效。

关于各规范背后的核心理念、原则和质量标准，请参阅
**[规范的编写方式](how-guidelines-are-written.md)**。

规范按照以下页面进行组织：

- **[通用规范](general-guidelines/index.html)** ——
  与语言无关的命名、注释等指导原则

- **[布局、格式与 API 设计规范](rust-guidelines/index.html)** ——
  Rust 相关指导原则，涉及命名、语言特性及交叉主题。
- **[Git 规范](git-guidelines.md)** ——
  提交规范与拉取请求约定。
- **[测试规范](testing-guidelines.md)** ——
  测试行为、断言、回归策略及清理工作。
- **[汇编规范](asm-guidelines.md)** ——
  `.S` 与 `global_asm!` 的约定，包括段划分、函数元数据、标签及对齐。

这些规范代表了代码库的**预期**状态。如果你在代码库中发现与此不符的情况，

欢迎你修正代码以符合规范。

## 索引

| 类别 | 规范 | 简称 |
|----------|-----------|------------|
| 通用 | 命名应具有描述性 | [`descriptive-names`](general-guidelines/index.html#descriptive-names) |
| 通用 | 命名应准确 | [`accurate-names`](general-guidelines/index.html#accurate-names) |
| 通用 | 在名称中编码单位和重要属性 | [`encode-units`](general-guidelines/index.html#encode-units) |
| 通用 | 使用断言风格的布尔型名称 | [`bool-names`](general-guidelines/index.html#bool-names) |
| 通用 | 优先使用语义换行 | [`semantic-line-breaks`](general-guidelines/index.html#semantic-line-breaks) |
| 通用 | 解释“为什么”，而非“是什么” | [`explain-why`](general-guidelines/index.html#explain-why) |
| 通用 | 记录设计决策 | [`design-decisions`](general-guidelines/index.html#design-decisions) |
| 通用 | 引用规范及算法来源 | [`cite-sources`](general-guidelines/index.html#cite-sources) |
| 通用 | 每文件仅关注一个概念 | [`one-concept-per-file`](general-guidelines/index.html#one-concept-per-file) |

| 通用 | 按自上而下的阅读顺序组织代码 | [`top-down-reading`](general-guidelines/index.html#top-down-reading) |
| 通用 | 将语句分组为逻辑段落 | [`logical-paragraphs`](general-guidelines/index.html#logical-paragraphs) |
| 通用 | 保持错误消息格式的一致性 | [`error-message-format`](general-guidelines/index.html#error-message-format) |
| 通用 | 遵循熟悉的约定 | [`familiar-conventions`](general-guidelines/index.html#familiar-conventions) |
| 通用 | 隐藏实现细节 | [`hide-impl-details`](general-guidelines/index.html#hide-impl-details) |
| 通用 | 在边界处验证，在内部信任 | [`validate-at-boundaries`](general-guidelines/index.html#validate-at-boundaries) |
| Rust | 遵循 Rust 的 CamelCase 和首字母缩写大小写规则 | [`camel-case-acronyms`](rust-guidelines/naming.md#camel-case-acronyms) |
| Rust | 以 `_fn` 结尾命名闭包变量 | [`closure-fn-suffix`](rust-guidelines/naming.md#closure-fn-suffix) |
| Rust | 引入解释性变量 | [`explain-variables`](rust-guidelines/language-items/variables-expressions-and-statements.md#explain-variables) |
| Rust | 使用块表达式限定临时状态的作用域 | [`block-expressions`](rust-guidelines/language-items/variables-expressions-and-statements.md#block-expressions) |
| Rust | 使用检查或饱和算术运算 | [`checked-arithmetic`](rust-guidelines/language-items/variables-expressions-and-statements.md#checked-arithmetic) |
| Rust | 最小化嵌套层级 | [`minimize-nesting`](rust-guidelines/language-items/functions-and-methods.md#minimize-nesting) |
| Rust | 保持函数小而专注 | [`small-functions`](rust-guidelines/language-items/functions-and-methods.md#small-functions) |
| Rust | 避免使用布尔类型参数 | [`no-bool-args`](rust-guidelines/language-items/functions-and-methods.md#no-bool-args) |
| Rust | 使用类型来强制执行不变性 | [`rust-type-invariants`](rust-guidelines/language-items/types-and-traits.md#rust-type-invariants) |

| Rust | 对于封闭集合优先使用枚举而非 trait 对象 | [`enum-over-dyn`](rust-guidelines/language-items/types-and-traits.md#enum-over-dyn) |
| Rust | 将字段封装在 getter 方法后 | [`getter-encapsulation`](rust-guidelines/language-items/types-and-traits.md#getter-encapsulation) |
| Rust | 遵循 RFC 1574 的摘要行规范 | [`rfc1574-summary`](rust-guidelines/language-items/comments-and-documentation.md#rfc1574-summary) |
| Rust | 以标点符号结束句子注释 | [`comment-punctuation`](rust-guidelines/language-items/comments-and-documentation.md#comment-punctuation) |
| Rust | 使用反引号包裹标识符 | [`backtick-identifiers`](rust-guidelines/language-items/comments-and-documentation.md#backtick-identifiers) |
| Rust | 不在文档注释中暴露实现细节 | [`no-impl-in-docs`](rust-guidelines/language-items/comments-and-documentation.md#no-impl-in-docs) |
| Rust | 为主要组件添加模块级文档 | [`module-docs`](rust-guidelines/language-items/comments-and-documentation.md#module-docs) |
| Rust | 论证每次 `unsafe` 使用的合理性 | [`justify-unsafe-use`](rust-guidelines/language-items/unsafety.md#justify-unsafe-use) |
| Rust | 文档化安全条件 | [`document-safety-conds`](rust-guidelines/language-items/unsafety.md#document-safety-conds) |
| Rust | 在 `kernel/` 中禁止 unsafe 代码 | [`deny-unsafe-kernel`](rust-guidelines/language-items/unsafety.md#deny-unsafe-kernel) |
| Rust | 在模块边界处推演安全性 | [`module-boundary-safety`](rust-guidelines/language-items/unsafety.md#module-boundary-safety) |
| Rust | 默认采用最窄的可见性 | [`narrow-visibility`](rust-guidelines/language-items/modules-and-crates.md#narrow-visibility) |
| Rust | 使用父模块限定函数调用 | [`qualified-fn-imports`](rust-guidelines/language-items/modules-and-crates.md#qualified-fn-imports) |
| Rust | 使用工作空间依赖 | [`workspace-deps`](rust-guidelines/language-items/modules-and-crates.md#workspace-deps) |
| Rust | 按字母顺序排列属性和派生 trait | [`alphabetical-attrs`](rust-guidelines/language-items/macros-and-attributes.md#alphabetical-attrs) |

| Rust | 在最窄作用域内抑制 lint | [`narrow-lint-suppression`](rust-guidelines/language-items/macros-and-attributes.md#narrow-lint-suppression) |
| Rust | 谨慎使用 `#[expect(dead_code)]` | [`expect-dead-code`](rust-guidelines/language-items/macros-and-attributes.md#expect-dead-code) |
| Rust | 优先使用函数而非宏 | [`macros-as-last-resort`](rust-guidelines/language-items/macros-and-attributes.md#macros-as-last-resort) |
| Rust | 建立并强制执行一致的锁顺序 | [`lock-ordering`](rust-guidelines/select-topics/concurrency-and-races.md#lock-ordering) |
| Rust | 持有自旋锁时绝不执行 I/O 或阻塞操作 | [`no-io-under-spinlock`](rust-guidelines/select-topics/concurrency-and-races.md#no-io-under-spinlock) |
| Rust | 不随意使用原子操作 | [`careful-atomics`](rust-guidelines/select-topics/concurrency-and-races.md#careful-atomics) |
| Rust | 临界区不得跨越锁边界拆分 | [`atomic-critical-sections`](rust-guidelines/select-topics/concurrency-and-races.md#atomic-critical-sections) |
| Rust | 仅用于正确性检查时使用 `debug_assert` | [`debug-assert`](rust-guidelines/select-topics/defensive-programming.md#debug-assert) |
| Rust | 使用 `?` 传播错误 | [`propagate-errors`](rust-guidelines/select-topics/error-handling.md#propagate-errors) |
| Rust | 仅使用 OSTD 日志宏 | [`ostd-log-only`](rust-guidelines/select-topics/logging.md#ostd-log-only) |
| Rust | 选择合适的日志级别 | [`log-levels`](rust-guidelines/select-topics/logging.md#log-levels) |
| Rust | 为每个 crate 定义日志前缀 | [`log-prefix`](rust-guidelines/select-topics/logging.md#log-prefix) |
| Rust | 所有资源获取与释放均使用 RAII | [`raii`](rust-guidelines/select-topics/memory-and-resource-management.md#raii) |
| Rust | 避免在热路径上使用 O(n) 算法 | [`no-linear-hot-paths`](rust-guidelines/select-topics/performance.md#no-linear-hot-paths) |
| Rust | 最小化不必要的拷贝和分配 | [`minimize-copies`](rust-guidelines/select-topics/performance.md#minimize-copies) |

| Rust | 无证据时不得进行过早优化 | [`no-premature-optimization`](rust-guidelines/select-topics/performance.md#no-premature-optimization) |
| Git | 使用祈使句撰写描述性主题行 | [`imperative-subject`](git-guidelines.md#imperative-subject) |
| Git | 每次提交只包含一个逻辑变更 | [`atomic-commits`](git-guidelines.md#atomic-commits) |
| Git | 将重构与功能开发分离 | [`refactor-then-feature`](git-guidelines.md#refactor-then-feature) |
| Git | 保持 Pull Request 聚焦 | [`focused-prs`](git-guidelines.md#focused-prs) |
| 测试 | 每个错误修复都添加回归测试 | [`add-regression-tests`](testing-guidelines.md#add-regression-tests) |
| 测试 | 测试用户可见行为而非内部实现 | [`test-visible-behavior`](testing-guidelines.md#test-visible-behavior) |
| 测试 | 使用断言宏而非人工检查 | [`use-assertions`](testing-guidelines.md#use-assertions) |
| 测试 | 每次测试后清理资源 | [`test-cleanup`](testing-guidelines.md#test-cleanup) |
| 汇编 | 使用正确的段指令 | [`asm-section-directives`](asm-guidelines.md#asm-section-directives) |
| 汇编 | 将代码宽度指令放在段定义之后 | [`asm-code-width`](asm-guidelines.md#asm-code-width) |
| 汇编 | 将属性直接放在函数之前 | [`asm-function-attributes`](asm-guidelines.md#asm-function-attributes) |
| 汇编 | 为可被 Rust 调用的函数添加 `.type` 和 `.size` | [`asm-type-and-size`](asm-guidelines.md#asm-type-and-size) |
| 汇编 | 使用唯一标签前缀避免名称冲突 | [`asm-label-prefixes`](asm-guidelines.md#asm-label-prefixes) |
| 汇编 | 优先使用 `.balign` 而非 `.align` | [`asm-prefer-balign`](asm-guidelines.md#asm-prefer-balign) |
