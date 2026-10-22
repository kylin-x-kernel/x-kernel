# DRM mode-setting ABI 回归用例

此目录是独立用户态 Cargo 包，不链接内核库，也不改变内核工作区的依赖。
用例通过真实文件描述符和真实 DRM ioctl 检查 legacy（非原子）mode-setting 的
可观察 ABI，适用于 64 位 X-Kernel Linux 用户环境。

`mode_setting_abi_regressions` 顺序覆盖：

- `CREATE_DUMB` 同时接受 32bpp 与 24bpp，且 24bpp 以同样的 32bpp 布局承载；
  宽度/高度为 0 与 bpp=16 被拒绝。
- dumb buffer 可 `MAP_DUMB`，且偏移页对齐。
- legacy `ADDFB`（28 字节）与 `ADDFB2`（104 字节）都能分配帧缓冲且各自独立；
  `ADDFB` 接受 bpp=24，`depth=0` 被拒绝。
- `MODE_CURSOR` 在真实 CRTC 上接受 MOVE 与 BO（含 `handle=0` 的隐藏请求），
  未知 CRTC 返回 `EINVAL`。
- `SET_CLIENT_CAP` 按 Linux 的 `_IOW` 编码：未知能力返回 `EOPNOTSUPP`，布尔值
  越界返回 `EINVAL`，并在同一个打开文件上可授予、可撤销；授予请求放在
  `mmap` 后经 `mprotect(PROT_READ)` 设为只读的页上，验证不会回写参数而误报 `EFAULT`。
- `DIRTYFB` 对未知帧缓冲返回 `ENOENT`，对已登记帧缓冲成功。
- 关闭并重新打开设备后上述接口仍可用（X server 重启会重建描述符）。

维护者可在提供标准 Rust Linux 用户态工具链的构建环境中，将本目录作为独立包构建：

```sh
cd io/drmdevice/abi-tests/drm-mode
cargo test --target aarch64-unknown-linux-musl --no-run
```

将生成的 `target/aarch64-unknown-linux-musl/debug/deps/drm_mode_regression-*`
可执行文件复制到 X-Kernel 用户环境，直接执行：

```sh
DRM_DEVICE=/dev/dri/card0 /usr/tests/drm_mode_regression --nocapture
```

该用例需要一个支持 DRM 的独立 guest，设备不能同时被 Xorg、合成器或其他客户端
占用。缺少设备或已有外部 owner 时会明确失败，不跳过断言。其余 64 位架构使用对应的
Linux musl target。

用例的 ioctl 请求号按 Linux uapi 的 `_IOWR`/`_IOW` 编码手写，并与驱动注册值逐字
一致；驱动侧结构体布局（`struct drm_mode_cursor` 的 `width`/`height`、
`struct drm_set_client_cap` 的只写方向）改动后，这里的常量必须同步，否则命令字
不会命中分发表，断言会因为“命令未注册”而假通过。
