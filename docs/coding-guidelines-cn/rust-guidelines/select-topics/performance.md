# 性能

我们非常重视关键路径的性能表现。
对热路径的修改必须进行基准测试。
不必要的数据拷贝、内存分配
以及 O(n) 算法均不被接受。

### 避免在热路径中使用 O(n) 算法（`no-linear-hot-paths`） {#no-linear-hot-paths}

系统调用分发、调度器入队
以及频繁的查询操作
不得引入 O(n) 复杂度，
其中 n 是可能较大的数量
（如进程数、文件描述符数量等）。
应要求采用亚线性的替代方案。

```rust
// 不好 —— 每次入队时进行 O(n) 扫描
fn select_cpu(&self, cpus: &[CpuState]) -> CpuId {
    cpus.iter()
        .min_by_key(|c| c.load())
        .expect("至少有一个 CPU")
        .id()
}

// 好 —— 维护一个优先级队列
// 使选择操作为 O(log n)
fn select_cpu(&self) -> CpuId {
    self.cpu_heap.peek().expect("至少有一个 CPU").id()
}
```

另请参阅：
PR [#1790](https://github.com/asterinas/asterinas/pull/1790)。

### 尽量减少不必要的拷贝与分配（`minimize-copies`）{#minimize-copies}

应避免多余的数据拷贝——
例如写入前先序列化到栈缓冲区、
在 `&` 引用已足够时克隆 `Arc`、
在迭代器可胜任时收集为 `Vec` 等操作。

```rust
// 不好 —— 不必要的 Arc::clone
fn process(&self, stream: Arc<DmaStream>) {
    let s = stream.clone();
    s.sync();
}

// 好 —— 在不需要所有权时使用借用
fn process(&self, stream: &DmaStream) {
    stream.sync();
}
```

另请参阅：
PR [#2582](https://github.com/asterinas/asterinas/pull/2582)
和 [#2725](https://github.com/asterinas/asterinas/pull/2725)。

### 无证据的过早优化不可接受（`no-premature-optimization`）{#no-premature-optimization}

性能优化必须由数据支撑。
引入复杂性来解决不存在的问题是不被接受的。
如果你声称某个改动提升了性能，
请展示相关数据。
