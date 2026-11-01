# DRM 渲染节点 / framebuffer 刷新 / 按引用释放 回归用例

此目录是独立用户态 Cargo 包，不链接内核库，也不改变内核工作区的依赖。
用例通过真实文件描述符和 DRM ioctl 检查可见 ABI，适用于 64 位 X-Kernel Linux 用户环境。

`render_node` 覆盖：

- `/dev/dri/renderD128` 存在且可读写打开；主节点 `card0` 与渲染节点同一设备号族（226）。
- 渲染节点 `GET_CAP` 成功；`CREATE_DUMB` 返回 `EACCES`（Linux 的 `DRM_RENDER_ALLOW` 白名单，
  不是 `ENOTTY`）。
- 渲染节点 `PRIME_HANDLE_TO_FD` 对未知 handle 返回 `EINVAL`——命令本身被节点允许，只是 handle
  不存在，用来区分"节点拒绝"与"handle 无效"。
- `DIRTYFB` 按 `fb_id` 查找 framebuffer：`fb_id == 0` 与任何未知 id 一样返回 `ENOENT`，
  已登记的 framebuffer 则冲刷成功（与 Linux 一致，Xorg 依赖该区分）。
- `DESTROY_DUMB` 在 framebuffer 仍引用该资源、且资源正在扫描输出时成功返回；随后 `RMFB`
  释放引用，再创建并提交新 framebuffer 仍可正常上屏（不刷黑）。
- handle 表按打开文件隔离：另一个描述符创建的 handle 在本描述符上 `GEM_CLOSE` 返回 `EINVAL`，
  且关闭持有 handle 的文件不影响本文件已登记的 framebuffer。

维护者可在提供标准 Rust Linux 用户态工具链的构建环境中，将本目录作为独立包构建：

```sh
cd io/drmdevice/abi-tests/drm-render-node
cargo test --target aarch64-unknown-linux-musl --no-run
```

将生成的 `target/aarch64-unknown-linux-musl/debug/deps/drm_render_node_regression-*`
可执行文件复制到 X-Kernel 用户环境，直接执行：

```sh
DRM_DEVICE=/dev/dri/card0 DRM_RENDER_NODE=/dev/dri/renderD128 \
    /usr/tests/drm_render_node_regression --nocapture
```

该用例需要一个支持 DRM 的独立 guest，设备不能同时被 Xorg、合成器或其他客户端占用。
缺少设备或已有外部 owner 时会明确失败，不跳过断言。其他 64 位架构使用对应的 Linux musl target。

内核按 Linux 语义只允许一个 master，因此多个测试必须串行执行（每个测试自己打开设备、
结束时释放 master）：

```sh
/usr/tests/drm_render_node_regression --nocapture --test-threads=1
```
