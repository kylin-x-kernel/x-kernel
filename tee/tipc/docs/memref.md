# tipc memref 实现说明

## 当前实现范围

`sys_tipc_memref_create(addr, size, prot)` 当前会为调用方提供的用户虚拟地址范围创建一个可传递的 TIPC handle。

安装 handle 之前，syscall 路径会完成这些校验：

- `size` 不能为 0。
- `addr + size` 不能溢出。
- `addr` 和 `size` 必须按页对齐。
- `prot` 只能包含支持的 `MMAP_FLAG_PROT_*` 位。
- `prot` 至少要包含一个访问权限位。
- 拒绝可执行映射。
- 拒绝 write-only 映射；可写 memref 必须同时可读。
- 当前进程地址空间必须完整覆盖 `[addr, addr + size)`。
- 覆盖该范围的 VMA 必须具备 `USER` 权限，以及 `prot` 请求的 `READ` 和/或 `WRITE` 权限。

校验通过后，`MemRef` 当前保存：

- 原始用户虚拟地址；
- 字节长度；
- 允许的 mmap protection mask；
- 普通 TIPC handle 状态。

创建出的 handle 可以通过 `send_msg` / `load_msg_handles` 作为 attached handle 放进 TIPC 消息，也可以在对端 `read_msg` 时安装到对端的 handle table。

因此，当前实现是一个带地址空间校验的 memref 元数据 handle。它还不是完整 Trusty 语义里的 memref object capability。

## Trusty 参考语义

Trusty C 里的 `sys_memref_create` 通过下面的路径创建 memref：

```c
memref_create_from_aspace(app->aspace, uaddr, size, mmap_prot, &handle)
```

其中最关键的一步是：

```c
vmm_get_obj(aspace, vaddr, size, &memref->slice)
```

`vmm_get_obj` 会把某个地址空间里的虚拟地址范围转换成稳定的 backing-object slice：

- 背后的 `vmm_obj`；
- 保证 backing object 存活的 object reference；
- object 内部的起始偏移；
- slice 长度。

这个 object slice 在发送方 unmap 原始虚拟地址之后仍然有意义。接收方之后可以把同一个 object slice mmap 到自己的地址空间中。发送方和接收方可以使用不同的虚拟地址，但背后指向同一个 backing object 范围。

## 与完整 Trusty 语义的差距

当前 X-Kernel 实现还没有提供这些 Trusty memref 语义：

- `MemRef` 没有保存稳定的 backing object 引用；
- 没有保存 object-relative offset；
- 发送方 unmap 原始地址后，`MemRef` 没有独立的 backing object 生命周期保障；
- 还没有实现接收方 mmap memref 的路径；
- object 级别权限校验目前只做到当前 VMA 权限校验；
- memref handle 还没有接入 object-side invalidate 或 revoke 行为。

换句话说，当前 handle 能证明发送方在创建时确实拥有一段合法映射，但它还没有携带“这段内存本身”的 backing object 能力。

## 需要 MM 层提供的能力

完整 Trusty-compatible memref 支持应该从 `memspace` 开始，而不是继续在 TIPC syscall adapter 里堆逻辑。

MM 层需要提供一个稳定的 exported slice 抽象，概念上类似：

```rust
pub struct VmObjectSlice {
    object: Arc<dyn ExportedVmObject>,
    offset: u64,
    size: usize,
    page_size: PageSize,
    max_flags: MappingFlags,
}
```

实际类型名可以不同，但这个抽象至少需要保证：

- slice 存活期间 backing object 不会被释放；
- slice 标识的是单个 object 范围，而不是单纯的 VMA identity number；
- object 能校验后续请求的 mmap 权限；
- object 能为另一个地址空间构造 VMA/runtime 映射；
- slice 的 offset 和 size 保持页对齐约束；
- private/COW 映射需要明确定义导出的到底是当前 anon result object、file source object，还是直接不支持。

`MmSpace` 应该提供类似这样的边界 API：

```rust
pub fn export_object_slice(
    &self,
    start: VirtAddr,
    size: usize,
    access_flags: MappingFlags,
) -> KResult<VmObjectSlice>;
```

这个 API 应该负责：

- 校验地址范围被 VMA 覆盖；
- 拒绝跨越多个不兼容 backing object 的范围；
- 计算请求虚拟地址范围对应的 object-relative offset；
- 获取 backing object 的强引用；
- 拒绝暂不支持的 backing kind；
- 当 object 不能支持请求权限时返回权限错误。

`tipc` 不应该直接查看 VMA/runtime 内部结构。它应该消费 `memspace` 提供的公开 export API，让 MM 层继续拥有 VMA 和 backing object 的不变量。

## MM 支持后 TIPC 需要怎么改

等 `memspace` 能导出 object slice 后，`MemRef` 应该从当前形式：

```rust
addr: usize,
size: usize,
mmap_prot: u32,
```

演进成更接近下面的形态：

```rust
slice: VmObjectSlice,
mmap_prot: u32,
```

之后 `sys_tipc_memref_create` 应该变成：

1. 校验 Trusty ABI 参数；
2. 把 `prot` 转换成 MM access flags；
3. 调用 `MmSpace::export_object_slice`；
4. 用返回的 slice 创建 `MemRef`；
5. 把 handle 安装到当前 TIPC handle table。

接收方 mmap memref 的路径也需要明确。可选方案包括：

- 给 `tipc_handle::Handle` 增加 mmap hook，专门由 memref handle 实现；
- 增加 Trusty-specific 的 handle mmap syscall 路径；
- 让 memref handle 适配现有 `posix/mm` 的 mmap mapper 模型。

无论选择哪条路径，最终都应该把 exported object slice 映射到接收方地址空间，并且映射权限同时受 memref 的 `mmap_prot` 和 object 自身策略约束。

## 建议分阶段推进

1. 保留当前经过 VMA 校验的 metadata 实现，作为临时兼容层。
2. 在 `memspace` 中设计并测试稳定 object slice 导出能力。
3. 先支持最简单的 backing kind，优先考虑 shared anonymous mapping。
4. 让 `MemRef` 持有 `VmObjectSlice`。
5. 实现接收方 mmap memref handle。
6. 在 COW、truncate、invalidate 和权限语义明确之后，再扩展到更多 backing kind。

private file-backed 和 private anonymous 映射需要特别谨慎。导出这些范围时，如果 MM 层没有定义稳定可导出的 object，可能会错误暴露一个正在变化的 COW 来源，而不是发送方创建 memref 时看到的私有内容。
