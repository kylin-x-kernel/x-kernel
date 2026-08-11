# 逻辑正确性审查规则

本阶段检查 PR 的控制流、状态转换、不变量、错误语义、unsafe 边界和测试完整性。
开始分析前必须继续读取 `docs/ai/review/common.md`。

本阶段与 Bug 阶段允许关注相邻问题，
但 Logic 阶段更强调“实现是否满足其声明的契约和状态模型”，
Bug 阶段更强调可直接触发的运行时故障。

## 相关规范路由

根据 diff 内容读取 `docs/ai/skills/code-guidelines/` 下对应文件：

| 变更类型 | 必读文件 |
|---|---|
| syscall、用户内存、current task | `boundaries-and-context.md` |
| unsafe、裸指针、FFI、`MaybeUninit` | `unsafety.md` |
| 锁、原子、wait/wake、跨 CPU 状态 | `concurrency.md` |
| `Result`、日志、panic、恢复路径 | `error-handling-and-logging.md` |
| public API、trait、状态表达 | `api-design.md` |
| 模块、可见性、feature、attribute | `modules-and-attributes.md` |
| 测试与提交完整性 | `git-and-testing.md` |

只读取与变更有关的主题文件，但命中主题时不能跳过。

## 强制工作流

1. 使用 `get_annotated_diff` 获取带行号 diff，并按功能路径分组。
2. 阅读所有关键变更文件的完整内容，而不是只看 hunk。
3. 搜索修改函数、类型、trait、状态字段和配置的调用方与其他实现。
4. 写出关键状态进入条件、退出条件和失败后的状态。
5. 分别检查正常、边界、错误、取消、并发和 Drop 路径。
6. 对 unsafe 或并发代码读取对应规范文件并逐项验证不变量。
7. 检查现有测试是否覆盖变更契约，而不只是是否新增了测试文件。
8. 仅报告由当前 diff 引入、能够定位且后果明确的问题。

## 重点检查项

### 1. 边界条件

系统性检查：

- 0、1、最大值、空集合和单元素集合；
- 区间的包含/排除端点；
- 页对齐前后、跨页、跨 chunk 和末尾不足一块；
- 第一个/最后一个 task、CPU、fd、VMA、timer 或队列元素；
- signed/unsigned 转换和负值；
- 重复调用、重复关闭、重复注册和幂等性；
- 部分成功、短读写、被信号中断和重试。

检查比较符号、循环范围和终止条件是否与契约一致。

### 2. 控制流与错误传播

检查：

- 条件是否反转或多个条件组合错误；
- `return`、`break`、`continue` 是否过早或遗漏；
- 新增分支是否使后续代码不可达；
- match 是否遗漏语义不同的 case，或用 `_` 吞掉未来变体；
- 错误类型、errno 和上层可观察行为是否一致；
- fallback 是否掩盖真实失败并提交错误状态；
- 重试和循环是否有明确退出条件；
- 异步/回调完成路径是否恰好执行一次。

不要仅凭“这里用了 `_` match”报告，
需要证明被合并的变体确实需要不同处理。

### 3. 状态一致性与生命周期

对修改多个字段或多个对象的操作检查：

- 字段是否按正确顺序同步更新；
- 读者能否观察到半初始化或半销毁状态；
- 失败回滚是否覆盖已经提交的每一步；
- 状态机是否遗漏合法转换或允许非法转换；
- owner、registry、引用计数和实际对象生命周期是否一致；
- publish 之前是否完成初始化，unpublish 之后是否仍有访问者；
- Drop、reap、close、unmap、detach 等清理动作是否只发生一次。

如果存在状态枚举或注释契约，应逐条对照实现，而不是只检查局部赋值。

### 4. 算法正确性

检查：

- 比较方向、排序稳定性和优先级；
- 索引、offset、长度和单位换算；
- bitmap 位号、mask、shift 和 endian；
- 树、队列、链表和区间合并/拆分不变量；
- hash key、相等性和去重语义；
- scheduler、timer、reference count 等算法是否保持单调性和守恒关系；
- 架构相关实现是否遵守指令、barrier 和寄存器语义。

必要时搜索其他架构或同类模块作为交叉验证，
但不能仅因为实现不同就判定当前实现错误。

## Unsafe 专项

diff 出现以下任一内容时，必须完整读取 `unsafety.md`：

- `unsafe {}`、`unsafe fn`、`unsafe trait`、`unsafe impl`；
- 裸指针解引用或 `ptr::read/write/copy`；
- `from_raw_parts`、`transmute`、union 字段；
- FFI、内联汇编、MMIO 或 volatile；
- `MaybeUninit`、手动 Drop、所有权重建；
- 自定义 `Send` / `Sync`。

### Unsafe 必查不变量

- 指针是否非空、有效、正确对齐并覆盖所需长度；
- 引用生命周期是否不超过底层对象；
- aliasing 是否满足共享/独占规则；
- 初始化状态与读取类型是否匹配；
- 所有权是否恰好重建一次，不会 double free；
- FFI ABI、布局、calling convention 和 unwind 边界是否正确；
- 并发访问是否满足 `Send` / `Sync` 和同步要求；
- arch/MMIO 操作的 barrier、宽度和顺序是否正确。

每个 unsafe block 前必须有紧邻的 `// SAFETY:` 注释，
注释需要说明调用者或当前代码如何维护上述真实不变量，不能只复述操作。
`unsafe fn` / `unsafe trait` 必须在 rustdoc 中包含 `# Safety`，明确调用者义务。

检查 unsafe 作用域是否最小。
如果项目已有安全封装，优先复用；
没有明显封装时才用 `search_crates` 检查成熟替代，
例如 `zeroize`、`zerocopy`、`safe-mmio` 或 `tock-registers`。

不要在不了解 no_std、架构和项目依赖约束时直接建议引入第三方 crate。

## 并发专项

diff 涉及共享状态时读取 `concurrency.md` 并检查：

- 锁获取顺序在所有路径上是否一致；
- guard 生命周期是否跨越 block、schedule、await 或外部回调；
- 检查条件和入队/休眠是否原子衔接，避免丢失唤醒；
- atomic ordering 是否发布了与标志关联的数据；
- per-CPU 状态是否在 migration/preempt/IRQ 条件下安全；
- 中断上下文是否使用可能睡眠的同步原语；
- teardown 是否等待并发读者退出；
- lock-free 路径是否有 ABA、回收和内存序问题。

报告原子内存序问题时，必须指出哪次写入需要被哪次读取观察，
不能只说 Ordering “太弱”。

## 测试完整性

测试不足只有在以下情况才应形成 finding：

- PR 修复了明确回归，却没有能在修复前失败的回归测试；
- 新增公共行为或边界语义，但现有测试没有覆盖关键分支；
- 修改 unsafe、状态机或并发不变量，却只测试 happy path；
- 新增架构/feature 分支，没有任何编译或运行覆盖路径；
- 测试断言过弱，实际上无法检测声称修复的问题。

不要机械要求每个 private helper 都新增单元测试。
优先建议最窄、可复现、能够区分修复前后的测试。

## 输出要求

遵循 `common.md` 的 finding JSON 格式。
评论需要指出被破坏的契约、不变量或状态转换，
并说明合法输入或真实执行路径如何到达该问题。
没有高置信度问题时输出 `未发现问题`。
