# fs_boot — 安全与可靠性分析

## 信任模型

root block device 名称来自内核构建配置；设备枚举和磁盘内容来自驱动及外部介质。
具体磁盘格式验证由所选 `FileSystemType` provider 负责。

## 外部边界 / 攻击面

- `KFEAT_ROOT_BLOCK` 可能不匹配已注册设备。
- block device 可能缺失、被移除、返回 I/O 错误或包含损坏的文件系统。
- 9P mount tag 和响应来自外部 transport。

本 crate 不处理用户指针；调用发生在用户进程启动前。

## unsafe 代码清单

`fs/boot/src` 没有 `unsafe` block。

## 内存安全不变量

- mount namespace 初始化成功后才能取得 initial root path。
- root device handle 在 mount provider 接管前保持有效。
- backing device id 的登记和移除在同一 mutex 下完成。

## 线程安全

初始 mount 建立由 boot CPU 串行执行。后续设备移除回调只在短 mutex 临界区更新 id
集合，不在持锁期间执行文件系统 I/O。

## 威胁分析

| 编号 | 威胁描述 | 影响等级 | 触发条件 | 应对措施 |
|------|----------|----------|----------|----------|
| T-01 | 错误 filesystem provider 被挂为 root | 高 | boot 自己按 feature/name 分支且与 Kconfig 漂移 | boot 只调用 exactly-one `fs_block::FileSystemType` |
| T-02 | 损坏磁盘触发不安全解析 | 高 | provider 未校验外部介质 | mount 错误必须传播；磁盘校验由具体 provider 实现 |
| T-03 | 已挂载 backing device 被移除 | 中 | 热移除 root 或 9P 设备 | 移除回调记录 stale 状态并告警，不伪装继续正常 |

## 故障模式与影响分析（FMEA）

| 编号 | 故障模式 | 故障原因 | 局部影响 | 系统影响 | 严重度 | 应对措施 |
|------|----------|----------|----------|----------|--------|----------|
| F-01 | 无 block device | 驱动未探测或配置错误 | 无法选择 root backing | boot panic | 1 | 明确 `No block device found` |
| F-02 | root mount 失败 | I/O、格式或 feature 错误 | 无 root superblock | boot panic | 1 | 记录 provider 错误后停止启动 |
| F-03 | 首选设备名不存在 | 配置与硬件不一致 | 使用 fallback 设备 | 可能选择非预期 root | 2 | 输出候选设备和 fallback warning |

## 故障管理

root/关键虚拟文件系统失败会停止启动，避免在不完整 namespace 上运行用户态。
可选 9P 和设备移除路径记录带设备身份的错误上下文。

## 已知限制

设备移除通知尚不能强制 mounted filesystem 进入只读或完成自动卸载；当前只报告
stale backing。root device fallback 仍保留兼容的发现顺序语义。

## 审计清单

- boot 是否仍然不引用具体 root filesystem crate 或 `KFEAT_FS_*` 分支？
- 新 mount path 是 namespace policy，还是应进入通用 mount/type 层？
- mount 失败是否在安装 namespace 前终止？
- 设备移除回调是否避免持锁执行 I/O？
