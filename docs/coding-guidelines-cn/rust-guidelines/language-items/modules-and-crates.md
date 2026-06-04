# 模块与 crate

### 按数据所有权与内聚度定义 crate 边界（`crate-boundaries`）{#crate-boundaries}

crate 的边界应首先由
哪些数据、状态、不变量、生命周期规则
以及执行上下文假设天然属于同一组来决定。
一个合适的 crate
应拥有一个高内聚的所有权域，
使其规则、审查范围与后续演进
都能在同一编译单元内清晰维护。

不要仅仅因为“现在放这里方便”，
就创建或保留一个 crate，
把无关的 helper、glue code
或迁移残留混在一起。
应避免职责松散的“大杂烩 crate”。

在 `mod` 和独立 crate 之间做选择时：

1. 如果某段代码只是某个 crate 的内部实现细分，
   并不承载多个同级 crate 共享依赖的所有权边界，
   那它更适合作为 `mod` 留在拥有者 crate 内部。
2. 如果同一个数据/状态边界、状态机、
   生命周期规则集合
   或承载不变量的组件
   会被多个同级 crate 依赖，
   那通常说明它具有 1 对 N 的关系，
   更适合作为独立 crate，
   明确其所有权边界。

简而言之：

- crate 用来承载一个高内聚的所有权域；
- `mod` 用来承载该所有权域内部的实现划分；
- 当多个同级 crate 都依赖同一个数据边界时，
  应优先将其拆成独立 crate。

### 默认使用最窄可见性（`narrow-visibility`）{#narrow-visibility}

从私有（private）开始，
仅在存在实际外部使用者时，
才将可见性扩展到 `pub(super)`、`pub(crate)` 或 `pub`。

```rust
// 良好——限制在父模块内
pub(super) static I8042_CONTROLLER:
    Once<SpinLock<I8042Controller, LocalIrqDisabled>> = Once::new();

pub(super) fn init() -> Result<(), I8042ControllerError> {
    // ...
}

// 不良——不必要的宽可见性
pub static I8042_CONTROLLER: ...
```

在 `aster-kernel` crate 内部，`pub(crate)` 与 `pub` 是等价的，
因为该 crate 没有下游使用者。
优先使用更简短的 `pub`。

另见：
PR [#2951](https://github.com/asterinas/asterinas/pull/2951)、
[#2605](https://github.com/asterinas/asterinas/pull/2605#discussion_r2720506912)
以及 [#3154](https://github.com/asterinas/asterinas/pull/3154#discussion_r3100905375)。

### 使用父模块限定函数调用（`qualified-fn-imports`）{#qualified-fn-imports}

当从另一个模块导入自由函数或静态/常量时，
应导入**父模块**并通过父模块来访问该条目。

通过父模块访问（`module::function()`、`module::CONSTANT`）。
不要直接按名称导入自由函数或静态/常量。

该规范得到了
[*The Rust Programming Language*](https://doc.rust-lang.org/book/ch07-04-bringing-paths-into-scope-with-the-use-keyword.html)
的推荐，并被 Rust 编译器代码库所遵循。
其目的有二：

1. 调用处能清晰表明
   使用的是导入的条目，
   而非本地条目。
2. 模块名称提供了上下文，
   对条目名称进行补充说明。

```rust
// 良好——通过父模块限定的函数调用
use ostd::irq;

let guard = irq::disable_local();

// 良好——通过父模块限定的静态访问
use ostd::mm::kspace;

let base = kspace::LINEAR_MAPPING_BASE_VADDR;

// 不良——裸露的函数名；调用处来源不明
use ostd::irq::disable_local;

let guard = disable_local();

// 不良——裸露的静态名；可能被误认为是本地常量
use ostd::mm::kspace::LINEAR_MAPPING_BASE_VADDR;

let base = LINEAR_MAPPING_BASE_VADDR;
```

该规则适用于**自由函数和静态/常量**。
类型、trait 和枚举变体
仍应直接按名称导入，
遵循标准的 Rust 惯例。

### 使用工作区依赖（`workspace-deps`）{#workspace-deps}

始终在工作区 `[workspace.dependencies]` 表中
声明共享依赖，
并在成员 crate 中通过 `.workspace = true` 引用它们。

```toml
# 在工作区根目录下的 Cargo.toml 中
[workspace.dependencies]
ostd = { version = "0.17.0", path = "ostd" }
bitflags = "2.6"

# 在成员 crate 的 Cargo.toml 中
[dependencies]
ostd.workspace = true
bitflags.workspace = true
```
