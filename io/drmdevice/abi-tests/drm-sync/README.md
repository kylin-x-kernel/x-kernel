# DRM 同步对象 / dma-buf 回归用例

此目录是独立用户态 Cargo 包，不链接内核库，也不改变内核工作区的依赖。
用例通过真实文件描述符与 DRM / dma-buf / sync_file ioctl 检查 G2 引入的
同步语义，适用于 64 位 X-Kernel Linux 用户环境：

- `SYNCOBJ_*` 命令字全部写死（`0xC0`–`0xCA`），并在 `drain_sync_defines_the_uapi_words`
  里重新推导 `repr(C)` 结构体尺寸（8/8/24/40/24/56/32/24、`sync_file_info` 56、
  `sync_fence_info` 80、`dma_buf_sync` 16），布局漂移只会让调用落到 `EOPNOTSUPP`，
  不会悄悄通过。
- `DRM_CAP_SYNCOBJ`、`DRM_CAP_SYNCOBJ_TIMELINE`、`DRM_CAP_PRIME` 只报告已经实现的能力。
- 二进制 syncobj：信号、`WAIT`、超时（成功返回且 `first_signaled == -1`）、`WAIT_ALL`、
  `HANDLE_TO_FD` 导出可读的 sync_file、`DESTROY` 后 `ENOENT`。
- timeline：按点信号、`QUERY` 报告最新可用点、更早的点同样可用、`RESET` 清空点。
- `TRANSFER` 与 `FD_TO_HANDLE` 把 fence 串到另一个对象；把卡片 fd 当作 sync_file 导入返回 `EINVAL`。
- 非法 flags、空对象数组、负超时都返回 `EINVAL`。
- PRIME：`PRIME_HANDLE_TO_FD` 返回真实 dma-buf fd；`DMA_BUF_IOCTL_INFO` 报告字节数；
  `DMA_BUF_IOCTL_SYNC` 的 START/NOWAIT 立即完成、方向为 0 返回 `EINVAL`；
  `DMA_BUF_IOCTL_EXPORT_SYNC_FILE` 的 sync_file 只有一条 fence 记录；
  同一 fd 再 `PRIME_FD_TO_HANDLE` 得到同一 backing；非 dma-buf fd 返回 `ENOENT`
  （对应 Linux 的 `dma_buf_get` 失败，"不支持"只用于确实是 dma-buf 但本卡导入不了的导出）；
  关掉 GEM handle 与卡片描述符后导出对象仍然有效。
- 从第二个卡片描述符导入同一个 dma-buf，用 `ADDFB2` 为导入对象建立 framebuffer
  （对象放得下即可，不要求与导出对象几何相同），并按写入方向做 `DMA_BUF_IOCTL_SYNC`
  的 START/END，再把 dma-buf 本身 `mmap` 出来。
- `MODE_ATOMIC`：平面暴露 `IN_FENCE_FD`（读回 `-1`）、CRTC 暴露 `OUT_FENCE_PTR`；
  卡片 fd 作为 `IN_FENCE_FD` 返回 `EINVAL`；`TEST_ONLY` 把 out-fence 字写成 `-1`；
  未映射的 `OUT_FENCE_PTR` 返回 `EFAULT`；一次真实提交返回的 out-fence fd 在翻页完成后
  可读、带状态 0 与完成时间戳。

构建与运行（与 `drm-gem-close` 相同）：

```sh
cd io/drmdevice/abi-tests/drm-sync
cargo test --target aarch64-unknown-linux-musl --no-run
```

把生成的可执行文件复制到 X-Kernel 用户环境执行：

```sh
DRM_DEVICE=/dev/dri/card0 /usr/tests/drm_sync_regression --nocapture
```

`dma_buf_exports_and_imports_one_backing` 不需要独占卡片；其余用例需要空闲卡片
（`SET_MASTER` 不被其他客户端持有）。
