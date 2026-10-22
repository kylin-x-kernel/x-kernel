# DRM GEM_CLOSE / MODE_CLOSEFB 回归用例

此目录是独立用户态 Cargo 包，不链接内核库，也不改变内核工作区的依赖。
用例通过真实文件描述符和 DRM ioctl 检查 `GEM_CLOSE`（0x09）与
`MODE_CLOSEFB`（0xD0）的可见行为，适用于 64 位 X-Kernel Linux 用户环境。

`gem_close_and_closefb` 顺序覆盖：

- 硬编码命令字 `0x40086409`（GEM_CLOSE）、`0xc00864d0`（CLOSEFB），并用本地
  `repr(C)` 尺寸重推 `_IOW`/`_IOWR` 断言相等（8 / 8 字节）；同时断言
  `CREATE_DUMB`、`ADDFB2`、`SETCRTC` 请求结构体的尺寸（32 / 104 / 104 字节）。
  命令字由尺寸编码而来，布局漂移不会编译失败，只会让调用落到 `EOPNOTSUPP`。
- `GEM_CLOSE`：**先校验 `pad`**——真实 handle 配非零 `pad` 返回 `EINVAL`、清掉 `pad` 后同一 handle 能正常关闭
  （顺序反过来实现也无法通过）；未知 handle → `EINVAL`（Linux
  `drm_gem_handle_delete()` 的返回值）；已创建的 dumb buffer → 返回 0；重复关闭 → `EINVAL`。
- `GEM_CLOSE` 请求放在 `mmap` 后经 `mprotect(PROT_READ)` 设为只读的页上时
  仍返回 0，且重复请求返回 `EINVAL`，避免已释放 handle 却因回写只读页而误报 `EFAULT`。
- `MODE_CLOSEFB`：非零 `pad` → `EINVAL`；未知 fb → `ENOENT`；
  fb 仍绑在 legacy CRTC 上 → `EBUSY`；解绑后 → 0；再次关闭 → `ENOENT`。
- `DESTROY_DUMB` 对未知 handle 仍返回 0（legacy 语义不变），用例顺带确认两条路径没有互相改坏。
- handle 先关闭、framebuffer 仍在用：`GEM_CLOSE` 返回 0 之后该 framebuffer 仍可被 `SETCRTC` 绑定
  （说明资源没有被提前销毁），随后 `MODE_CLOSEFB` 对仍绑定的它返回 `EBUSY`，解绑后再关闭才成功；
  此时资源只是**进入退役**，要等宿主切到下一个扫描目标（下一次 present 时的 sweep）才真正释放。

构建与运行方式（与 `drm-identity`、`drm-set-property` 相同）：

```sh
cd io/drmdevice/abi-tests/drm-gem-close
cargo test --target aarch64-unknown-linux-musl --no-run
```

把生成的可执行文件复制到 X-Kernel 用户环境执行：

```sh
DRM_DEVICE=/dev/dri/card0 /usr/tests/drm_gem_close_regression --nocapture
```

该用例需要一个支持 DRM 的独立 guest，设备不能同时被 Xorg、合成器或其他客户端占用。
