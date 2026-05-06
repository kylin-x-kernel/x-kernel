---
# 宏与属性

### 按字母顺序排列属性与派生 trait (`alphabetical-attrs`) {#alphabetical-attrs}

当一个条目带有多个外部属性时，应**按名称的字母顺序**排列非派生属性，并将 `#[derive(...)]` 放在**最后**。
在 `#[derive(...)]` 内部，也应**按字母顺序**排列各个 trait。

```rust
// 正确——非派生属性已排序；derive 放在最后且其内部 trait 已排序
#[cfg(feature = "alloc")]
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Pod)]
pub struct Foo { ... }

// 错误——任意排序
#[derive(Debug, Default, Clone, Copy, Pod)]
#[cfg(feature = "alloc")]
#[repr(C)]
pub struct Foo { ... }
```

将 `#[derive(...)]` 放在**最后**，可确保派生宏始终在所有属性宏（例如 `#[padding_struct]`、`#[pod_union]`）完成对条目的转换之后才看到它。派生辅助属性（例如 `#[serde(...)]`、`#[clap(...)]`）应紧跟在 `#[derive(...)]` 之后。将剩余属性按字母顺序排序，可消除放置时的犹豫，并减少差异中的噪音。

另请参阅：
PR [#3080](https://github.com/asterinas/asterinas/pull/3080#discussion_r3031834321)

---
(讨论动机)  
以及 PR [#2898](https://github.com/asterinas/asterinas/pull/2898#discussion_r2763969731)  
(此前临时决定的排序方式).

### 谨慎使用 `#[expect(dead_code)]` (`expect-dead-code`) {#expect-dead-code}

通常情况下，应避免死代码，因为  
_(i)_ 它会引入不必要的维护开销，  
_(ii)_ 其正确性只能通过手动且易出错的方式审查来保证。

仅当以下所有条件都满足时，死代码才是可接受的：

1. 未来将实现一个**具体场景**，使这段死代码变为存活代码。

2. 即使没有具体用例，其**语义**也足够清晰。
3. 死代码**足够简单**，提交者和审查者都能在未经测试的情况下确信其正确性。
4. 它作为已有存活代码的对应部分存在。

例如，添加未被使用的 ABI 常量是可以接受的，因为对应的功能仅实现了部分。

另请参阅：
[Rust 参考手册：诊断属性](https://doc.rust-lang.org/reference/attributes/diagnostics.html)
以及 rustc [`unfulfilled_lint_expectations`](https://doc.rust-lang.org/rustc/lints/listing/warn-by-default.html#unfulfilled-lint-expectations)。

### 在最窄作用域内抑制 lint (`narrow-lint-suppression`) {#narrow-lint-suppression}

当抑制 lint 时，
抑制的范围应尽可能小。
这能让读者了解
触发 lint 的确切位置，
并便于后续提交者
维护该抑制。

```rust
// Good — 每个方法单独标记
trait SomeTrait {
    #[expect(dead_code)]
    fn foo();

    #[expect(dead_code)]
    fn bar();

    fn baz();
}

// Bad — 整个 trait 被抑制
#[expect(dead_code)]
trait SomeTrait { ... }
```

有一个例外情况：
如果足够明确每个成员都会触发该 lint，
那么在类型级别上进行预期抑制是合理的。

```rust
#[expect(non_camel_case_types)]
enum SomeEnum {
    FOO_ABC,
    BAR_DEF,
}
```

另请参阅：

[Clippy `allow_attributes`](https://rust-lang.github.io/rust-clippy/master/#allow_attributes)，
[Clippy `allow_attributes_without_reason`](https://rust-lang.github.io/rust-clippy/master/#allow_attributes_without_reason)，
以及 rustc [`unfulfilled_lint_expectations`](https://doc.rust-lang.org/rustc/lints/listing/warn-by-default.html#unfulfilled-lint-expectations)。

### 优先使用函数而非宏（`macros-as-last-resort`）{#macros-as-last-resort}

优先使用函数和泛型，而非宏。
宏虽然功能强大，
但难以理解、调试、测试和格式化。
仅在类型系统或泛型无法表达
所需功能时
（例如可变参数、编译时代码生成、
或 DSL 语法），才考虑使用宏。

```rust
// Good — 一个泛型函数即可覆盖所有类型
fn align_up<T: Into<usize>>(val: T, align: usize) -> usize {
    let val = val.into();
    (val + align - 1) & !(align - 1)
}

// Bad — 本可用函数实现，却用了宏
macro_rules! align_up {
    ($val:expr, $align:expr) => {
        ($val + $align - 1) & !($align - 1)
    };
}
```

另请参阅：
《Rust 程序设计语言》第 20.5 章“宏”；  
[Rust 实例教程：宏](https://doc.rust-lang.org/rust-by-example/macros.html).
