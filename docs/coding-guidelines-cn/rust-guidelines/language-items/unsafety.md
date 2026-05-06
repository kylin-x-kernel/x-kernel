# Unsafety

### 为每个 `unsafe` 的使用提供理由 (`justify-unsafe-use`) {#justify-unsafe-use}

每个 `unsafe` 代码块前都必须有一个 `// SAFETY:` 注释，用以说明该操作是安全的。对于包含多个条件的不变量，请使用编号列表：

```rust
// SAFETY:
// 1. 我们拥有对当前上下文和下一个上下文的独占访问权（见上文）。
// 2. 下一个上下文是有效的（因为它要么已正确初始化，要么由之前的
//    `context_switch` 写入）。
unsafe {
    context_switch(next_task_ctx_ptr, current_task_ctx_ptr);
}
```

---

另请参阅：
PR [#2958](https://github.com/asterinas/asterinas/pull/2958)
和 [#836](https://github.com/asterinas/asterinas/pull/836)。

### 记录安全性条件 (`document-safety-conds`) {#document-safety-conds}

所有 `unsafe` 函数和 trait
都必须在文档注释中包含一个 `# Safety` 部分，
用以描述调用方必须满足的条件、性质或不变量。
明确指出调用方必须保证的内容——
而非实现细节或副作用。

```rust
/// 一个用于强制原子模式的守卫类型的标记 trait。
///
/// # Safety
///
/// 实现者必须确保在守卫类型存活期间原子模式得以维持。
pub unsafe trait InAtomicMode: core::fmt::Debug {}
```

### 在 `kernel/` 中禁止 unsafe 代码 (`deny-unsafe-kernel`) {#deny-unsafe-kernel}

`kernel/` 下的所有 crate 都必须禁止 unsafe 代码：

```rust
#![deny(unsafe_code)]
```

仅允许 OSTD（`ostd/`） crate 包含 `unsafe` 代码。
如果某个内核 crate 需要执行不安全操作，
相关功能应在 OSTD 中作为安全的 API 提供。

### 在模块边界处进行安全性推理 (`module-boundary-safety`) {#module-boundary-safety}

`unsafe` 代码块的安全性取决于所有能够访问同一私有状态的代码。将不安全抽象封装在尽可能小的模块中，以最小化"审计面"。同一模块内任何可能修改所依赖字段的代码，都是安全性论证的一部分。

```rust
// 好——小而专注的模块限制了审计面
mod frame_allocator {
    /// 不变式：`next` 始终是一个有效的帧索引。
    struct FrameAlloc {
        next: usize,
        // ...
    }

    impl FrameAlloc {
        pub fn alloc(&mut self) -> PhysAddr {
            // SAFETY：`next` 始终有效（参见上面的不变式）。
            // 只有此模块中的代码可以修改 `next`。
            unsafe { self.alloc_frame_unchecked(self.next) }
        }
    }
}
```
