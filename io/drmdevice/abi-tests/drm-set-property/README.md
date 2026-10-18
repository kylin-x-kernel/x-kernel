# DRM property 回归用例

此目录是独立用户态 Cargo 包，不链接内核库，也不改变内核工作区的依赖。
用例通过真实文件描述符和 DRM ioctl 检查 `MODE_SETPROPERTY`（0xAB）与
`MODE_OBJ_SETPROPERTY`（0xBA）的可见 ABI，适用于 64 位 X-Kernel Linux 用户环境。

`property_entry_points_are_reachable` 顺序覆盖：

- 硬编码命令字 `0xc01064ab` / `0xc01864ba`，并用本地 `repr(C)` 结构体尺寸重新推导
  `_IOWR` 编码后断言两者相等（16 / 24 字节）；`MODE_GETRESOURCES`、
  `MODE_OBJ_GETPROPERTIES` 的结构体尺寸同样断言（64 / 32 字节）。
  命令字由尺寸编码而来，布局漂移不会编译失败，只会让客户端调用落到 `EOPNOTSUPP`，
  所以这一条只能靠调用验证。
- 未知 connector → `ENOENT`；已知 connector + 未知 property → `EINVAL`。
- 连接器 `CRTC_ID` 属性写入既非 0 也非当前 CRTC 的对象值 → `EINVAL`。
- 成功路径：把读到的当前值写回 → 返回 0。
- 对象形式：`obj_id` 与 `obj_type` 不一致 → `ENOENT`；未知对象 → `ENOENT`；
  未知属性 → `EINVAL`；非法值 → `EINVAL`；成功路径 → 0。
- 用例结束时把连接器属性恢复为开始时读到的值，重复运行不改变设备状态。

维护者可在提供标准 Rust Linux 用户态工具链的构建环境中，将本目录作为独立包构建：

```sh
cd io/drmdevice/abi-tests/drm-set-property
cargo test --target aarch64-unknown-linux-musl --no-run
```

将生成的 `target/aarch64-unknown-linux-musl/debug/deps/drm_set_property_regression-*`
可执行文件复制到 X-Kernel 用户环境，直接执行：

```sh
DRM_DEVICE=/dev/dri/card0 /usr/tests/drm_set_property_regression --nocapture
```

该用例需要一个支持 DRM 的独立 guest，设备不能同时被 Xorg、合成器或其他客户端占用。
缺少设备时用例会明确失败，不跳过断言。
