# evdev 回归用例

独立的用户态 Cargo 包，检查 evdev 的 ioctl 分发层：libinput 打开一个输入
设备时会做的探测（`EVIOCGVERSION`、`EVIOCGID`、`EVIOCGNAME`、`EVIOCGPHYS`、
`EVIOCGPROP`、`EVIOCGKEY`、`EVIOCGBIT`、`EVIOCGABS`）都必须按请求编码的
长度回答，而不是按任意长度。

- 命令字全部写死为 Linux 的值，布局漂移会让调用落到 `EINVAL` 而不是悄悄通过；
- 名字必须非空（libinput 以名字标识设备）；
- 属性/按键位图回答的长度等于请求长度（零填充）；
- 未知轴号返回 `EINVAL`；
- 没有 `EV_ABS` 的设备对 `EVIOCGABS` 返回 `EINVAL`，绝对轴设备则返回真实范围。

构建与运行：

```sh
cd io/drmdevice/abi-tests/evdev
cargo test --target aarch64-unknown-linux-musl --no-run
```

把生成的可执行文件复制到 X-Kernel 用户环境执行：

```sh
EVENT_DEVICE=/dev/input/event0 /usr/tests/evdev_regression --nocapture
```
