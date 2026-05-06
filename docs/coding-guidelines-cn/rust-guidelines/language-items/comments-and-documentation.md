# 注释与文档

API文档用于描述API的含义和用法，并通过 `rustdoc` 生成渲染。公共API（包括 crate、模块、结构体、trait、函数和宏）都应提供文档。`#![warn(missing_docs)]` lint 有助于强制执行这一基本规范。

Asterinas 遵循 Rust 社区文档编写惯例。两个主要参考来源如下：
1. rustdoc 手册：
   [如何编写文档](https://doc.rust-lang.org/rustdoc/how-to-write-documentation.html)
2. Rust RFC 手册：
   [API 文档编写规范](https://rust-lang.github.io/rfcs/1574-more-api-documentation-conventions.html#appendix-a-full-conventions-text)

---
### 遵循 RFC 1574 摘要行约定（`rfc1574-summary`）{#rfc1574-summary}

文档注释的第一行应简洁且为一个完整的句子。其语法形式取决于被注释项的类型：

- **函数和方法** — 使用第三人称单数现在时主动语态动词（"Returns"、"Creates"、"Acquires"），描述所执行的操作。
- **类型（结构体、枚举、trait、类型别名）、模块和字段** — 使用能命名该事物的名词短语，而不是描述某个动作。这与 Rust 标准库的惯例一致（例如，`Vec` 的文档为 "A contiguous growable array type"）。

```rust
/// Returns the mapping's start address.
pub fn map_to_addr(&self) -> Vaddr {
    self.map_to_addr
}

/// A policy for how [`FsPath::from_fd_at`] treats an empty `path_str`.
pub enum EmptyPathStr { /* ... */ }

/// A guard that releases a [`SpinLock`] when dropped.
pub struct SpinLockGuard<'a, T> { /* ... */ }
```

### 用标点结束句子注释（`comment-punctuation`）{#comment-punctuation}

如果注释行是一个完整的句子，请用适当的标点结束。这能改善密集代码中的可读性，并避免出现支离破碎的表述。

```rust
// 良好 — 完整句子且带标点
// SAFETY: The pointer is derived from a live allocation.

// 不良 — 完整句子但无标点
// SAFETY: The pointer is derived from a live allocation
```

### 将标识符用反引号包裹（`backtick-identifiers`）{#backtick-identifiers}

文档注释中的类型名、方法名和代码标识符应使用反引号包裹，以便 `rustdoc` 正确渲染。在引用类型时，尽量优先使用 `rustdoc` 链接（`[TypeName]`）。

```rust
/// Acquires the [`SpinLock`] and returns a guard
/// that releases the lock on [`Drop`].
///
/// Callers must not call `acquire` while holding
/// a [`RwMutex`] to avoid deadlock.
pub fn acquire(&self) -> SpinLockGuard<'_, T> { ... }
```

### 不在文档注释中披露实现细节（`no-impl-in-docs`）{#no-impl-in-docs}

文档注释应描述 API **做什么**以及**如何使用**，而不是**内部如何实现**。

```rust
// 良好 — 面向行为
/// 返回活跃连接的数量。

// 不良 — 泄露实现细节
/// 返回用于按套接字地址跟踪连接的内部 `HashMap` 的长度。
```

### 为主要组件添加模块级文档（`module-docs`）{#module-docs}

作为重要内核组件（例如子系统入口点、主要数据结构、驱动程序）的模块文件，应以 `//!` 注释开头，解释以下内容：
1. 该模块的功能
2. 其公开的关键类型
3. 它与相邻模块的关系

```rust
//! 虚拟内存区域（VMA）管理。
//!
//! 本模块定义了 [`VmMapping`] 及相关类型，
//! 这些类型表示进程虚拟地址空间中的连续区域。
//! VMA 由父模块中的 [`Vmar`] 树管理。
```
