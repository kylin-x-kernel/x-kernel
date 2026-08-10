# fs_boot — 安全与可靠性分析

## 信任模型

root block device 名称来自内核构建配置；设备枚举和磁盘内容来自驱动及外部介质。
具体磁盘格式验证由所选 `RootFileSystem` provider 负责。
内建 filesystem type 描述符和 boot mount policy 是受信任的内核构建产物。

## 外部边界 / 攻击面

- `KFEAT_ROOT_BLOCK` 可能不匹配已注册设备。
- block device 可能缺失、被移除、返回 I/O 错误或包含损坏的文件系统。
- 9P mount tag 和响应来自外部 transport。

本 crate 不处理用户指针；调用发生在用户进程启动前。

## unsafe 代码清单

`fs/boot/src` 没有 `unsafe` block。

## 内存安全不变量

- bootstrap mount namespace 初始化成功后才能取得 initial root path。
- root device handle 在 `FsContext::get_tree` 和真实 root graft 完成前保持有效。
- init `FsStruct` 的 root/pwd 在 bootstrap 和真实 root 两次切换中都成对更新。
- 所有内建 filesystem type 必须在安装用户态可见的 procfs 和开放 mount syscall 前
  完成唯一注册。
- `nosuid`、`nodev`、`noexec`、`relatime` 属于具体 `Mount`；不得写入共享
  superblock 状态。`/dev` 不得设置 `nodev`。
- boot 挂载的 devtmpfs/sysfs 必须与对应 type factory 使用同一共享 superblock，
  否则后续 mount 会暴露缺失 boot 节点的分离目录树。
- devtmpfs/sysfs 构造器必须保留 internal root mount 的 active 引用；可见 mount 卸载不得
  teardown 内核仍在更新的共享树。

## 线程安全

初始 mount 建立由 boot CPU 串行执行。后续设备移除回调只在短 mutex 临界区更新 id
集合，不在持锁期间执行文件系统 I/O。

## 威胁分析

| 编号 | 威胁描述 | 影响等级 | 触发条件 | 应对措施 |
|------|----------|----------|----------|----------|
| T-01 | 错误 filesystem provider 被挂为 root | 高 | boot 自己按 feature/name 分支且与 Kconfig 漂移 | boot 只调用 exactly-one `fs_block::RootFileSystem` |
| T-02 | 损坏磁盘触发不安全解析 | 高 | provider 未校验外部介质 | mount 错误必须传播；磁盘校验由具体 provider 实现 |
| T-03 | 已挂载 backing device 被移除 | 中 | 热移除 root 或 9P 设备 | mount 持有 resident object；后续 I/O 传播设备错误，9P 移除另行告警 |
| T-04 | filesystem type 列表与用户 mount 分派漂移 | 中 | boot、procfs 和 syscall 各自维护类型名 | boot 只向 KVFS 注册描述符；procfs 和 POSIX mount 都读取该注册表 |
| T-05 | 启动 mount flags 错误作用到共享 superblock 或禁用 `/dev` | 高 | 把 per-mount flags 填入 statfs 后端状态，或给 `/dev` 设置 `nodev` | boot 通过 namespace attach 设置 `MountFlags`；superblock 构造器只接收 filesystem-wide flags |

## 故障模式与影响分析（FMEA）

| 编号 | 故障模式 | 故障原因 | 局部影响 | 系统影响 | 严重度 | 应对措施 |
|------|----------|----------|----------|----------|--------|----------|
| F-01 | 无 block device | 驱动未探测或配置错误 | 无法选择 root backing | boot panic | 1 | 明确 `No block device found` |
| F-02 | root mount 失败 | source lookup、I/O、格式或 feature 错误 | 无真实 root superblock | boot panic | 1 | 记录 `FsContext/get_tree` 错误后停止启动 |
| F-03 | 首选设备名不存在 | 配置与硬件不一致 | 无法选择 root | boot panic | 2 | 输出配置名和候选设备，不回退到其他介质 |
| F-04 | filesystem type 重名或重复注册 | 启动接线错误 | 注册返回 `ResourceBusy` | boot 在 namespace 对用户态开放前停止 | 2 | 每个配置启用的 canonical type 恰好注册一次 |

## 故障管理

root/关键虚拟文件系统失败会停止启动，避免在不完整 namespace 上运行用户态。
可选 9P 和设备移除路径记录带设备身份的错误上下文。

## 已知限制

设备移除尚不能强制 mounted block filesystem 进入只读或完成自动卸载；已持有对象的
后续 I/O 会返回底层错误。未配置 root name 时仍保留按 `dev_t` 排序的兼容选择。filesystem type
当前是静态内建集合，不支持运行时注册/卸载模块。

## 审计清单

- boot 是否仍然不引用具体 root filesystem crate 或 `KFEAT_FS_*` 分支？
- 新 mount path 是 namespace policy，还是应进入通用 mount/type 层？
- 新 filesystem 是否注册 canonical type，且没有在 procfs 或 POSIX 层增加名称表？
- 新启动 mount 的 flags 是否写入 `Mount`；`/dev` 是否保持可访问设备节点？
- 真实 root mount 失败是否在启动用户态前终止，并保留明确错误？
- 设备移除回调是否避免持锁执行 I/O？
