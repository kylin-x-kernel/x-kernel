---
# 变量、表达式和语句

### 引入解释性变量（`explain-variables`）{#explain-variables}

通过将中间结果赋值给命名良好的变量来分解复杂表达式。解释性变量能将晦涩的表达式转化为自文档化的代码：

```rust
// 良好——意图清晰
let is_page_aligned = addr % PAGE_SIZE == 0;
let is_within_range = addr < max_addr;
debug_assert!(is_page_aligned && is_within_range);

// 糟糕——读者必须解析整个表达式
debug_assert!(addr % PAGE_SIZE == 0 && addr < max_addr);
```

---

另请参阅：
《编写可读代码的艺术》第 8 章“分解巨型表达式”；
PR [#2083](https://github.com/asterinas/asterinas/pull/2083#discussion_r2512772091)
与 [#643](https://github.com/asterinas/asterinas/pull/643#discussion_r1497243812)。

### 使用块表达式限定临时状态的作用域（`block-expressions`）{#block-expressions}

当临时变量仅用于产生一个最终值时，应使用块表达式。
这能将临时状态局部化，
避免一次性名称泄露到外部作用域。

```rust
// 良好——中间值的作用域被限定在块内
let socket_addr = {
    let bytes = read_bytes_from_user(addr, len as usize)?;
    parse_socket_addr(&bytes)?
};
connect(socket_addr)?;

// 糟糕——临时变量泄露到外部作用域
let bytes = read_bytes_from_user(addr, len as usize)?;
let socket_addr = parse_socket_addr(&bytes)?;
connect(socket_addr)?;
```

### 使用检查式或饱和算术运算（`checked-arithmetic`）{#checked-arithmetic}

对于可能溢出的运算，应使用检查式或饱和算术运算。优先选择显式的溢出处理，而非静默的环绕：

```rust
// 良好——显式处理溢出
let total = base.checked_add(offset)
    .ok_or(Error::new(Errno::EOVERFLOW))?;

// 良好——进行截断而非环绕
let remaining = budget.saturating_sub(cost);

// 糟糕——发布构建中可能静默环绕
let total = base + offset;
```

如果环绕行为是有意为之，应使用显式的 `wrapping_*` 或 `overflowing_*` 操作，并记录为什么允许环绕是正确的。
