# memfd-test — 设计文档

## 定位与范围

该用户态回归直接通过 `SYS_memfd_create` 进入 syscall，不模拟内核名称校验。
它覆盖 `ict/shared-buffer` 和 `/../buffer`，修复前内核对首个带 `/` 的名称
返回 `EINVAL`，程序退出非零；修复后还必须通过后续对象可用性检查。

## 流程与执行上下文

程序为单线程 Linux ABI 用户进程，无需 root、procfs 或全局临时路径。
每个标签创建两个 fd，第二个标签用 `:` 替换 `/`，在 X-Kernel 内部格式化后
与第一个同名。分别 `ftruncate` 到 4096 字节，将第一个以 `MAP_SHARED` 映射，
写入已知内容、解除映射后通过 `pread` 核对；第二个必须仍返回零数据。
它验证 syscall 接受标签、匿名对象可用、同名 backing 不共享内容。

这不是内核私有 mount 拓扑检查，也不要求 `/proc/self/fd` 与 Linux 的显示名
逐字一致。内核 formatter 单元断言只验证单分量格式，不替代本回归。

## 构建与运行

在已配置平台与独立测试镜像上运行：

```sh
make uapps UAPPS=memfd-test
# 在测试 guest 中手动运行：
/usr/local/bin/memfd-test
```

manifest 使用平台提供的 `CROSS_COMPILE`，静态链接，无新增 Cargo 依赖。
没有 autostart，不能把安装成功当作执行成功。只有退出 0 并输出
`memfd slash syscall regression: PASS` 才算本程序通过。Linux 参考与
X-Kernel 修复前后必须分别记录源码、二进制与实际退出码。

## 资源释放

所有失败路径汇合到清理逻辑，释放已创建的映射和两个 fd；不使用全局 hook、
线程、fork 或固定文件名。清理失败同样使程序退出非零。
