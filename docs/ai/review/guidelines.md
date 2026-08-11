# X-Kernel 编码规范审查规则

本阶段进行“规范合规、代码库一致性和 API/模块边界”审查。
目标不是泛泛检查格式，而是发现会增加维护成本、扩大安全审计面、
破坏项目约定或形成错误抽象的具体问题。

开始分析前必须读取：

1. `docs/ai/review/common.md`；
2. `docs/ai/skills/code-guidelines/SKILL.md`；
3. 根据本文件路由出的相关主题文件。

`code-guidelines/SKILL.md` 是编码规范的权威入口。
本文件定义如何在自动 review 中应用它，不复制并替代完整规范。

## 强制工作流

### 1. 分类变更

使用 `get_annotated_diff` 获取 diff，按下列维度标记文件：

- Rust 实现；
- public API、trait 或模块入口；
- unsafe / FFI / 汇编；
- 并发和共享状态；
- syscall、用户内存和 current-context；
- `Cargo.toml`、Kconfig、feature 和 attribute；
- 测试；
- rustdoc、`design.md`、`security.md` 和工作流文档。

### 2. 加载相关规范

| 变更内容 | 必读主题文件 |
|---|---|
| 命名、函数结构、可读性 | `naming-and-structure.md` |
| 注释、rustdoc、模块文档 | `comments-and-rustdoc.md` |
| public API、trait、参数和状态表达 | `api-design.md` |
| syscall、用户内存、current-context | `boundaries-and-context.md` |
| 模块、可见性、import、macro、attribute | `modules-and-attributes.md` |
| unsafe、裸指针、FFI、`MaybeUninit` | `unsafety.md` |
| 锁、原子、wait/wake、IRQ 并发 | `concurrency.md` |
| `Result`、panic、日志和恢复 | `error-handling-and-logging.md` |
| hot path、资源生命周期和复杂度 | `performance-and-resources.md` |
| 回归测试、commit 完整性 | `git-and-testing.md` |
| `.S`、`global_asm!`、内联汇编 | `assembly.md` |

命中某个主题就完整读取对应文件，不能只依赖本文件的摘要。

### 3. 阅读上下文

- Rust 文件必须使用 `read_file` 阅读完整内容。
- 修改 `Cargo.toml` 时读取当前 crate manifest；涉及共享依赖时读取 workspace 根 manifest。
- 新增 public item 或模块入口时读取相邻 `lib.rs`、`mod.rs` 和父模块。
- 修改 arch 私有实现时搜索同架构相邻模块和其他架构对应实现。
- 修改设计、安全或生命周期时读取 crate 下现有 `docs/design.md` 和 `docs/security.md`。

### 4. 搜索代码库惯例

使用 `search_content` / `search_files` 确认：

- 是否已有等价 helper、trait、RAII guard 或安全封装；
- 同一 crate 如何命名、传播错误和声明依赖；
- 同类 public API 使用何种可见性与模块层级；
- 架构相关操作是否已有统一 HAL；
- 新增抽象是否真的存在第二个使用者或扩展点。

只有搜索后才能评论“应复用已有实现”或“与代码库惯例不一致”。

### 5. 限定 finding

只报告：

- 违反明确规范；
- 位于 PR 新增或修改行；
- 对可维护性、安全审计、API 稳定性或代码库一致性有实际影响；
- 能给出具体修复方向。

不要报告 rustfmt 能自动处理的格式问题或纯个人偏好。

## 重点检查项

### 1. 模块所有权与可见性

检查：

- 新增 `pub` 是否真的需要跨 crate 暴露；
- 仅供 crate 或父模块使用的 item 是否应为 `pub(crate)` / `pub(super)` / private；
- 类型和 helper 是否放在真正拥有其状态、不变量和生命周期的 crate；
- 公共能力是否错误地放在 arch 私有、platform 私有或具体驱动目录；
- 公共接口是否泄露实现类型、内部字段或锁；
- 拆 crate 与保留 `mod` 的选择是否符合复用和所有权边界；
- 是否重新引入 broad catch-all crate 或跨层反向依赖。

X-Kernel 当前重构强调职责归属。
评论应指出具体 owner 和合理层级，而不是笼统要求“抽象一下”。

### 2. 复用已有抽象

新增下列代码前必须搜索项目已有实现：

- cache/TLB/CPU 指令和 barrier；
- 锁、IRQ guard、preempt guard 和 RAII 清理；
- MMIO、volatile、用户内存和地址转换；
- 位图、队列、区间、状态机和引用管理；
- 错误类型、日志前缀和配置解析；
- 架构 HAL、driver framework 和资源管理。

项目已有封装能满足契约时，应复用而不是重复实现。
但不要为了表面 DRY 合并拥有不同不变量或不同演进方向的代码。

### 3. 第三方 crate 搜索

diff 出现内联汇编、手写底层 CPU 操作或通用数据结构/算法时，
在确认项目内没有合适封装后，使用 `search_crates` 检查成熟方案。

例如：

- AArch64 barrier / system register：检查 `aarch64-cpu` 等 no_std 方案；
- volatile 访问：检查项目现有 MMIO 抽象及成熟 volatile crate；
- bitflags：检查 workspace 是否已有 `bitflags`；
- 敏感数据清零：检查 `zeroize`；
- 字节布局解析：检查 `zerocopy`；
- 自定义同步原语：先检查 X-Kernel 锁层，再评估 no_std crate。

查看 crate source 和 feature，确认 API、no_std、架构和许可证适用后才能建议。
不能只凭 crates.io 名称要求新增依赖。

### 4. 最小必要复杂度

审查新增 trait、dyn dispatch、泛型、宏、builder、缓存、配置项、状态机和 helper 时，
按以下顺序判断：

1. 能否完全不引入；
2. 能否复用项目现有实现；
3. 能否用 `core` / `alloc` 或语言原生能力；
4. 能否使用已有依赖；
5. 最后才接受新的最小实现。

高信号问题包括：

- 只有一个实现且没有真实扩展点的 trait；
- 只服务一个调用点、反而隐藏控制流的 helper/builder；
- 为尚不存在的需求加入配置层和缓存失效机制；
- 手写标准集合已有的算法；
- 引入跨模块状态只为减少几行代码。

不要为了缩短代码牺牲正确性、类型安全、错误处理或审计边界。

### 5. 命名与结构

检查：

- 名称是否准确表达真实副作用和成本；
- 类型无法表达时，名称是否包含 bytes/pages/ns 等单位；
- boolean 是否使用 `is_`、`has_`、`can_` 等断言式命名；
- 持有闭包/函数的变量是否符合项目 `_fn` 约定；
- 已实际使用的参数是否仍保留误导性的 `_` 前缀；
- 函数是否混合 syscall 语义、状态管理和底层字节操作；
- 嵌套、控制流和文件布局是否符合自顶向下阅读。

重命名等单行机械修复按 `common.md` 提供 suggestion。

### 6. Cargo、feature 与 attribute

检查：

- workspace 已声明的依赖是否使用 `.workspace = true`；
- 可能被多个成员使用的新依赖是否应由 workspace 统一管理；
- feature 是否在 crate、workspace、Kconfig 和调用点之间正确传递；
- `cfg` 是否导致未覆盖或语义不一致的构建组合；
- attribute 是否缩小到必要 item，而不是掩盖整个模块的 warning；
- 新依赖是否适用于 no_std、目标架构和项目许可要求。

### 7. Unsafe 与安全边界

命中 unsafe 时以 `unsafety.md` 的场景目录和 review 问题为准。
重点检查：

- unsafe 是否必要，能否使用项目安全封装；
- block 是否最小化；
- `SAFETY:` 是否陈述有效性、对齐、生命周期、aliasing、初始化和并发不变量；
- `unsafe fn` / `unsafe trait` 是否有完整 `# Safety`；
- public safe API 是否真的封装住不安全前置条件；
- raw pointer、FFI、`MaybeUninit`、所有权重建和 `Send/Sync` 是否使用最窄 sound pattern；
- 设计或安全文档是否需要同步更新。

不要接受“调用这里是安全的”一类循环论证注释。

### 8. 并发与执行上下文

检查 API 是否明确依赖：

- current process thread 或仅 current execution path；
- IRQ disabled、preempt disabled 或 CPU-local pinning；
- early boot、scheduler、allocator 和内存映射已初始化；
- 可以 sleep/block，还是必须在 atomic/interrupt context 运行；
- 锁顺序、重入和回调约束。

检查锁作用域、原子 ordering、wake/block 交接和资源销毁。
上下文前置条件应由类型、API 边界或准确文档表达，不能只存在于作者假设中。

### 9. 错误处理与日志

检查：

- 普通错误是否使用 `?` 和 typed error 传播；
- `.unwrap()` / `.expect()` 是否会把合法运行时状态升级为 kernel panic；
- `let _ =`、`.ok()` 或默认值是否静默吞错；
- 有意忽略错误时是否说明原因并保留必要日志；
- 日志级别、前缀和内容是否符合 crate 惯例；
- 热路径是否新增高频日志或昂贵格式化；
- 错误消息是否具体并遵守项目格式。

### 10. 注释、rustdoc 与模块文档

检查：

- 注释解释“为什么”和不明显的约束，而不是复述代码；
- 设计决策和外部规范是否引用来源；
- public API 是否说明参数、返回值、errors、panics 和 safety；
- 标识符是否用反引号，完整句子是否有标点；
- 行为、签名或错误变化后，相邻文档是否同步；
- 架构、状态机、生命周期、并发模型变化是否更新 `docs/design.md`；
- trust boundary、unsafe、不变量或失败处理变化是否更新 `docs/security.md`。

文档同步 finding 必须指出哪个现有描述已过期或缺少哪个新契约，
不能只说“建议补充文档”。

### 11. 测试与变更完整性

检查：

- bug fix 是否有能复现旧行为的回归测试；
- public behavior 和边界条件是否有覆盖；
- unsafe、并发和错误路径是否只测 happy path；
- 架构/feature 改动是否有对应构建或运行验证；
- refactor 与功能变化是否混在一起导致难以验证；
- 相关 rustdoc、设计、安全和测试是否与实现一起完成。

测试建议必须具体到要验证的行为和预期结果。

## 不应报告的情况

- 仅仅偏好另一种命名或代码排列，现有写法没有违反规范；
- 要求把所有 private item 都加文档；
- 为假想复用点提前抽象；
- 建议引入第三方 crate 但没有验证项目适用性；
- 只因其他架构实现不同就要求统一；
- 把大规模重构作为修复一条局部规范问题的前提；
- 评论未修改的历史代码；
- rustfmt、编译器或 lint 已能准确自动处理的问题。

## 输出要求

遵循 `common.md` 的 finding JSON 格式。
每条评论应明确指出违反的规范名称或主题，说明实际维护/安全影响，并给出具体建议。
对重命名、可见性收窄、workspace 依赖、下划线参数、单行错误处理和注释同步，
能可靠单行修复时必须提供 suggestion。
没有明确规范问题时输出 `未发现问题`。
