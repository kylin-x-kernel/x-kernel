# DRM identity 回归用例

此目录是独立用户态 Cargo 包，不链接内核库，也不改变内核工作区的依赖。
用例通过真实文件描述符和 DRM ioctl 检查可见 ABI，适用于 64 位 X-Kernel Linux 用户环境。

`identity_buffers_and_master_lifecycle` 顺序覆盖：

- VERSION 三个字符串字段的零容量、空指针、短缓冲和完整长度回报，以及缓冲区前后保护字节。
- GET_UNIQUE 未设置身份时不访问目标、容量不足时不写入、容量足够时完整复制且不越界。
- 非 owner 的 SET_VERSION 协商和查询均返回 EACCES，且不建立共享身份。
- owner 查询及接口 1.0 不建立身份，接口 1.4 建立身份。
- owner 请求接口 1.2 时回填最高支持版本 1.4。
- 第二个独立打开的文件 SET_MASTER 返回 EBUSY。
- DROP_MASTER 后身份保留，原 owner 重新获取时复用原身份。
- dup 描述符维持所有权，最后一个 owner 描述符关闭后其他文件可以获取所有权。
- client 成为 owner 时身份为空，最后关闭后新打开的文件成为 owner。

维护者可在提供标准 Rust Linux 用户态工具链的构建环境中，将本目录作为独立包构建：

```sh
cd io/drmdevice/tests
cargo test --target aarch64-unknown-linux-musl --no-run
```

将生成的 `target/aarch64-unknown-linux-musl/debug/deps/drm_identity_regression-*`
可执行文件复制到 X-Kernel 用户环境，直接执行：

```sh
DRM_DEVICE=/dev/dri/card0 /usr/tests/drm_identity_regression --nocapture
```

该用例需要一个支持 DRM 的独立 guest，设备不能同时被 Xorg、合成器或其他客户端占用。
缺少设备或已有外部 owner 时会明确失败，不跳过断言。其他 64 位架构使用对应的 Linux musl target。
