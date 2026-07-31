# kernel-image-metadata — 安全与可靠性分析

## 信任模型

XKMake 构造的字段值属于受信构建输入，但待修改 ELF 的 section、note header、
program header 和文件范围均需验证。kernel 侧把镜像 descriptor 视为可能损坏
的数据，不在完成边界、CRC32 和 UTF-8 校验前打印。

## 外部边界 / 攻击面

host 边界是 Cargo 产出的 ELF 文件；kernel 边界是 linker symbol 描述的启动
镜像内存。crate 不接触用户内存、MMIO、DMA、设备、文件系统或网络输入。

## unsafe 代码清单

`linker_slice` 将 linker symbol 地址转换为静态 slice。其安全性依赖最终
linker script 将 start/end symbol 包围的固定 reserve 放在同一已初始化、
只读且已加载的 `PT_LOAD` 范围内；函数先验证地址顺序和非零长度，再调用
`from_raw_parts`。XKMake 在 finalize 与 bundle 复用时验证完整 reserve，不能
只验证缩短后的可见 note section。

## 内存安全不变量

- linker descriptor 的 start 不大于 end，范围在已映射 kernel image 内。
- 所有编码字段使用显式小端字节序，不依赖 Rust ABI 或结构体布局。
- payload 结束位置使用 checked arithmetic 和 slice `get` 验证。
- UTF-8 校验成功后才返回 `&str`。

## 线程安全

kernel image 在启动前完成写入，运行时只读。crate 无内部同步和全局可变状态。

## 威胁分析

| 编号 | 威胁描述 | 影响等级 | 触发条件 | 应对措施 |
|------|----------|----------|----------|----------|
| T-01 | ELF 字段伪造导致 host 越界写 | 高 | 输入 ELF section/header 损坏 | XKMake 验证 section 文件范围、大小和 load segment 包含关系 |
| T-02 | linker range 错误导致 kernel 越界读 | 高 | linker script 与 crate ABI 不一致 | 固定 symbol 名、检查地址顺序，并在 build 阶段验证 section |
| T-03 | 损坏 payload 被作为字符串打印 | 中 | 镜像字节被破坏 | CRC32 和 UTF-8 均通过后才返回字符串 |
| T-04 | CRC32 被误作真实性保证 | 低 | 镜像存在恶意篡改 | CRC32 仅检测意外损坏；真实性由安全启动或镜像签名承担 |

## 故障模式与影响分析（FMEA）

| 编号 | 故障模式 | 故障原因 | 局部影响 | 系统影响 | 严重度 | 应对措施 |
|------|----------|----------|----------|----------|--------|----------|
| F-01 | payload 超出预留空间 | 字段增长 | bundle 构建失败 | 不发布新 bundle | 3 | 编码返回 `PayloadTooLarge` |
| F-02 | note 不在 load segment | linker 规则回归 | ELF 后处理失败 | 不生成启动镜像 | 2 | XKMake 显式验证 program header 包含关系 |
| F-03 | 启动校验失败 | ELF 损坏或协议不匹配 | 元数据不可用 | 内核继续启动并打印错误 | 4 | entry 将元数据视为诊断信息 |

## 故障管理

格式 API 全部通过 `Result` 返回。host 失败会中止临时 bundle 的提升；kernel
读取失败只影响诊断输出，不阻止后续初始化。

## 隐私分析

payload 可包含构建用户名和主机名。正式发布可以通过构建环境覆盖或清除该字段；
该信息不应被视为运行时安全决策输入。

## 已知限制

- CRC32 不提供抗篡改能力。
- format version 1 只支持 UTF-8 文本 payload。
- build-info descriptor 固定为 1024 字节。

## 审计清单

- 修改 note、header 或 symbol 名时是否同步 producer、consumer 与 linker？
- 新 ELF 写路径是否验证 section 在 `PT_LOAD` 内？
- Build ID 是否在所有 loadable 内容完成后最后写入？
- kernel 是否始终把读取失败作为可恢复诊断错误？
