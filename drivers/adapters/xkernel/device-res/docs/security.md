# device-res-xkernel — 安全与可靠性分析

## 信任模型

```text
device-res provider contract
        │ validated resource descriptors and handler objects
        ▼
device-res-xkernel
        │
        ├── memspace MMIO mapping
        ├── kirq IRQ registration / MSI-X
        └── kdma DMA allocation / mapping
```

`device-res-xkernel` 信任：

- `memspace::iomap_device` 拒绝非法 MMIO 物理地址范围；
- `kirq` 校验 IRQ descriptor、管理 shared action lifecycle、in-flight
  synchronization 和 teardown；
- `kdma` 返回配对的 coherent/streaming DMA allocation metadata；
- `khal::time::monotonic_time()` 返回单调不倒退的时间；
- driver remove 路径在 devres cleanup 前已经停止设备 DMA 和中断源。

`device-res-xkernel` 不暴露 driver-facing `devm_*` helper，也不直接安装全局
provider；资源申请必须通过 `device_res` provider contract。`kdriver::resource`
持有静态 `XKernelResourceProvider` 实例，显式传给 `device_res::devm_*_with_provider()`
和 VirtIO PCI/MSI-X 等需要直接 provider 能力的内部适配路径。

## Unsafe 边界

### DMA allocation

`alloc_coherent()` 通过 `Layout::from_size_align()` 验证 `DmaSpec` 后调用
`kdma::allocate_dma_memory()`。

不变量：

- layout 非零且对齐合法；
- 返回的 buffer 由 `device_res::DmaCoherent` 或 devres cleanup 独占；
- 释放时使用同一个 `DmaSpec` 重建 layout。

### DMA free

`free_coherent()` 从 `DmaAllocation` 重建 `kdma::DMAInfo` 并调用
`kdma::deallocate_dma_memory()`。

不变量：

- `DmaAllocation` 来自本 provider 的 `alloc_coherent()`；
- 每个 allocation 只释放一次；
- 驱动已停止可能访问该 buffer 的设备 DMA。

### Streaming DMA map/unmap

`map_streaming()` 和 `unmap_streaming()` 根据 `DmaDirection` 调用 `kdma` streaming API。

不变量：

- 调用者提供的 buffer 在 mapping 生命周期内有效；
- `unmap_streaming()` 消费的是同一 provider 返回的 `DmaMapping`；
- direction 与原 mapping 一致。

## 并发与生命周期

- Provider 自身无可变共享状态；`kdriver::resource` 持有静态 provider 引用并在
  资源申请时显式传递。
- IRQ action list 同步由 `kirq` 保护。
- `TimeOp::monotonic_time()` 不维护本 crate 状态，只转发到 X-Kernel 时间源。
- Devres cleanup 按 `DeviceObject` LIFO 顺序执行；释放 IRQ 时会通过 `kirq` 等待旧
  hardirq snapshot 退出。

## 已知限制

- `device_res` 已预留 threaded IRQ provider contract，但当前 X-Kernel provider
  基于 main 分支的 `kirq` 只支持 shared hardirq request；threaded request 仍返回
  `ResError::Unsupported`。
- MSI-X 仅在 x86_64 backend 上实现，其他架构返回 `ResError::Unsupported`。
