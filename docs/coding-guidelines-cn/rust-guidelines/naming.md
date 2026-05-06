# 命名

Asterinas 在整个代码库中强制执行符合 Rust 惯用风格的命名规范。
名称必须准确、不缩写，
并遵循
[Rust API 命名指南](https://rust-lang.github.io/api-guidelines/naming.html)。

### 遵循 Rust 驼峰命名与首字母大写缩略词 (`camel-case-acronyms`) {#camel-case-acronyms}

类型名称遵循 Rust 的驼峰命名约定。
根据 Rust API 指南，缩略词采用首字母大写形式：

```rust
// 推荐
IoMemoryArea
PciDeviceLocation
Nvme
Tcp

// 不推荐
IOMemoryArea
PCIDeviceLocation
NVMe
TCP
```

### 以 `_fn` 结尾的闭包变量 (`closure-fn-suffix`) {#closure-fn-suffix}

存储闭包或函数指针的变量必须以 `_fn` 结尾，以表明它们是可调用的。
将闭包变量视为数据对象会误导读者。

```rust
// 好——明确表示是可调用对象
let task_fn = self.func.take().unwrap();
let thread_fn = move || {
    let _ = oops::catch_panics_as_oops(task_fn);
    current_thread!().exit();
};

let expired_fn = move |_guard: TimerGuard| {
    ticks.fetch_add(1, Ordering::Relaxed);
    pollee.notify(IoEvents::IN);
};
```

另见：
PR [#395](https://github.com/asterinas/asterinas/pull/395#discussion_r1402964415)
和 [#783](https://github.com/asterinas/asterinas/pull/783#discussion_r1593335375)。
