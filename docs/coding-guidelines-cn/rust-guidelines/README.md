---
# Rust 编码规范

Asterinas 遵循 [Rust API 指南](https://rust-lang.github.io/api-guidelines/) 以及以下项目特定约定。

- **[命名规范](naming.md)** ——
  驼峰命名/缩写风格
  及闭包变量后缀。
- **[语言项](language-items/index.html)**
  - **[变量、表达式与语句](language-items/variables-expressions-and-statements.md)** ——
    说明变量、
    块表达式
    及检查算术。
  - **[函数与方法](language-items/functions-and-methods.md)** ——

- **[嵌套控制、函数焦点及布尔参数回避](language-items/functions-and-methods.md)**  
- **[类型与 Trait](language-items/types-and-traits.md)** ——  
  类型层面的不变性、
  用于封闭集合的枚举、
  以及字段封装。  
- **[注释与文档](language-items/comments-and-documentation.md)** ——  
  RFC 1574 摘要、
  注释/文档风格、
  以及模块文档。  
- **[Unsafe 安全性](language-items/unsafety.md)** ——  
  `// SAFETY:` 正当性验证、
  `# Safety` 文档说明、
  以及模块边界推理。

- **[模块与 Crate](language-items/modules-and-crates.md)** ——  
    可见性控制  
    及工作区依赖关系。  
  - **[宏与属性](language-items/macros-and-attributes.md)** ——  
    限定性 lint 抑制、  
    `#[expect(dead_code)]` 策略  
    及宏使用克制。  
- **[专题讨论](select-topics/index.html)**  
  - **[并发与竞态](select-topics/concurrency-and-races.md)** ——  
    锁顺序、  
    自旋锁规范、  
    原子操作  
    及临界区。  
  - **[防御性编程](select-topics/defensive-programming.md)** ——  
    `debug_assert!` 的正确使用。

- **[错误处理](select-topics/error-handling.md)** ——  
  使用 `?` 进行错误传播。
- **[日志记录](select-topics/logging.md)** ——  
  标准日志宏及日志级别选择。
- **[内存与资源管理](select-topics/memory-and-resource-management.md)** ——  
  RAII 与所有权驱动的清理。
- **[性能](select-topics/performance.md)** ——  
  热点路径复杂度、  
  拷贝/分配控制、  
  以及基于证据的优化。
