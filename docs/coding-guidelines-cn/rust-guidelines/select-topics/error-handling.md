# 错误处理

Rust 的错误处理模型——`Result`、`?` 操作符以及类型化错误——是编写可靠内核代码的核心。

### 使用 `?` 传播错误（`propagate-errors`）{#propagate-errors}

使用 `?` 操作符以惯用的方式传播错误。在内核代码中，凡是可能发生合理失败的地方，都禁止使用 `.unwrap()`。

```rust
// 正确——使用 ? 传播错误
let tsc_info = cpuid.get_tsc_info()?;
let frequency = tsc_info.nominal_frequency()?;

// 错误——unwrap 隐藏了失败路径
let tsc_info = cpuid.get_tsc_info().unwrap();
```

参见：
《Rust 编程语言》第9章“错误处理”
以及 [Rust 通过示例学习：使用 `?` 解包 Option 和默认值](https://doc.rust-lang.org/rust-by-example/std/result/question_mark.html)。
