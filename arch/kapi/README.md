# kapi - Kernel Inter-Processor Communication API

X-Kernel 的多核处理器间通信（IPC）模块，基于硬件 IPI 机制实现。

## 功能特性

- ✅ **Per-CPU 队列**：每个 CPU 独立队列，减少锁竞争
- ✅ **异步执行**：发送方不阻塞，接收方中断上下文执行
- ✅ **类型安全**：`Send`/`Sync` trait 保证跨 CPU 安全
- ✅ **错误处理**：日志记录机制，便于调试
- ✅ **简洁 API**：3 个核心函数即可完成所有操作

## 使用示例

```rust
use kapi;

// 初始化（在每个 CPU 启动时调用）
kapi::init();

// 在指定 CPU 执行任务
kapi::run_on_cpu(3, || {
    println!("Running on CPU 3");
}).unwrap();

// 在所有 CPU 广播执行
kapi::run_on_each_cpu(|| {
    println!("Running on all CPUs");
}).unwrap();
```

## 错误处理

当前实现采用**日志记录方案**：
- 无效 CPU ID 会记录错误并返回 `Err(KapiError::InvalidCpuId)`
- 回调执行失败会记录日志但不中断其他回调

未来可选优化：
- 引入完成信号机制（`AtomicResult`）
- 支持 `catch_unwind` 捕获 panic

## 依赖关系

- `axhal`：硬件 IPI 中断触发
- `platconfig`：平台 CPU 数量配置
- `percpu`：Per-CPU 变量支持

## 架构设计

详见模块内文档注释和 X-Kernel 架构文档。
