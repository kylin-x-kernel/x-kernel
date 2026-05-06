# 防御性编程

断言用于验证程序正确性所必须保持不变的不变量。选择合适的断言类型可以在安全性和运行时开销之间取得平衡。

### 对于纯粹的正确性检查使用 `debug_assert`（`debug-assert`）{#debug-assert}

验证正确代码中不应失败的不变量的断言应放入 `debug_assert!` 而非 `assert!`。
`debug_assert!` 在发布构建中会被编译掉，因此该检查可以在开发过程中捕获错误，同时在生产环境中不产生任何额外开销。

```rust
debug_assert!(self.align.is_multiple_of(PAGE_SIZE));
debug_assert!(self.align.is_power_of_two());
```

另请参阅：
[std::debug_assert!](https://doc.rust-lang.org/std/macro.debug_assert.html)
和 [Rust 参考手册：`debug_assertions`](https://doc.rust-lang.org/reference/conditional-compilation.html#debug_assertions)。
