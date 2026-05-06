# 内存与资源管理

Rust 的所有权模型是内核中安全资源管理的主要工具。

### 对所有资源的获取与释放使用 RAII（`raii`） {#raii}

资源——包括中断启用/禁用状态、端口号、文件句柄、DMA 缓冲区、锁守卫——必须使用 `Drop` trait 实现自动清理。不允许使用手动的 `enable()`/`disable()` 成对调用方式。

```rust
// 正确 — RAII 守卫确保中断会重新启用
fn disable_local() -> DisabledLocalIrqGuard { ... }

impl Drop for DisabledLocalIrqGuard {
    fn drop(&mut self) {
        enable_local_irqs();
    }
}

// 错误 — 调用者可能忘记重新启用
fn disable_local_irqs() { ... }
fn enable_local_irqs() { ... }
```

### 优先使用词法生命周期（`lexical lifetimes`）  
让 Rust 编译器自动插入 `drop` 调用，而非手动调用 `drop()`。  
当默认的析构顺序不正确时，才使用显式的 `drop()` 调用。  

参考：  
PR [#164](https://github.com/asterinas/asterinas/pull/164)。
