---
# 日志记录

一致的日志记录方式能够使大型内核代码库的调试工作变得易于管理。

### 仅使用 OSTD 日志宏 (`ostd-log-only`) {#ostd-log-only}

所有基于 OSTD 的 crate 必须使用 [`ostd::log`] 模块提供的日志宏：
`debug!`、`info!`、`notice!`、`warn!`、`error!`、
`crit!`、`alert!`、`emerg!`。
通过 `use ostd::prelude::*` 或 `use ostd::log::{info, warn, ...}` 导入这些宏。

请勿直接使用第三方的 [`log`](https://docs.rs/log) crate。

---
OSTD 提供了一个桥接层，用于转发来自使用 `log` crate 的第三方 crate（例如 `smoltcp`）的消息，但第一方代码必须使用 OSTD 的宏。

自定义输出函数、`println!` 以及手写的串口打印宏，在生产代码中是不允许的。  
例外情况：在日志子系统初始化之前运行的代码，可以使用早期启动时的输出辅助工具。

```rust
// 正确：使用 OSTD 的宏
info!("VirtIO 块设备初始化完成: {} 个扇区", num_sectors);

// 错误：直接使用 log crate
log::info!("VirtIO 块设备初始化完成: {} 个扇区", num_sectors);

// 错误：使用 println
println!("VirtIO 块设备初始化完成: {} 个扇区", num_sectors);
```

[`ostd::log`]: https://asterinas.github.io/ostd/ostd/log/

### 选择合适的日志级别（`log-levels`）{#log-levels}

OSTD 提供八个日志级别，对应 [`syslog(2)`] 中描述的严重级别：

| 级别 | 用途 |
|-------|---------|
| `emerg!` | 系统不可用；在 `abort()` 之前立即使用。 |
| `alert!` | 必须立即采取行动。 |
| `crit!` | 关键条件：不可恢复的资源耗尽。 |
| `error!` | 严重但可恢复的失败：违反不变量、I/O 错误。 |
| `warn!` | 可恢复的问题：使用回退路径、检测到弃用用法。 |

| `notice!` | 正常但重要的事件：CPU 上线、安全功能激活。 |
| `info!` | 常规信息性事件：子系统初始化、配置变更。 |
| `debug!` | 开发诊断：状态转换、中间值、逐包追踪。 |

对于系统可恢复的失败，使用 `error!`。
仅当系统即将停止或中止时，才使用 `crit!` 或 `emerg!`。
每次系统调用或每次定时器滴答都会触发的日志语句，必须使用 `debug!`。

[`syslog(2)`]: https://man7.org/linux/man-pages/man2/syslog.2.html

### 为每个 crate 定义日志前缀（`log-prefix`）{#log-prefix}

每个基于 OSTD 的 crate 必须在 crate 根（`lib.rs` 中），在任何 `mod` 声明之前，定义一个 `__log_prefix` 宏。

这将为来自该 crate 的所有日志消息添加标签：

```rust
// Set this crate's log prefix for `ostd::log`.
macro_rules! __log_prefix {
    () => {
        "virtio: "
    };
}
```

约定：使用小写的 crate 名称（不带 `aster_` 前缀），后跟 `: `。
例如：`"virtio: "`、`"pci: "`、`"uart: "`。

crate 内部的子系统模块可以覆盖此前缀。

通过在 `mod.rs` 顶部定义自己的 `__log_prefix` 来实现：

```rust
// 为此模块设置 `ostd::log` 的日志前缀。
macro_rules! __log_prefix {
    () => {
        "net: "
    };
}
```

子模块会自动继承此覆盖。

不要在 `__log_prefix` 定义上添加 `#[rustfmt::skip]` 或其他属性——这会导致编译器歧义错误（E0659）。

不要手动使用像 `[IOMMU]` 或 `[Virtio]:` 这样的括号前缀。`__log_prefix` 机制已经取代了它们。
