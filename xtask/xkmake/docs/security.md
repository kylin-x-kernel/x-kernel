# xkmake — 安全与可靠性分析

## 信任模型

`xkmake` 是 host 侧开发工具，信任仓库源码，但把 CLI、环境变量、
`.config`、文件路径和附加 QEMU 参数视为需要校验的本地输入。

## 外部边界 / 攻击面

- 调用 Cargo build/rustdoc、交叉 GCC/binutils、`rust-objcopy`、FAT/mtools、
  QEMU、`debugfs`、cargo-shear、licensure 和 Rust LLVM coverage tools；
- 读取用户指定的配置、仓库根目录 `linker.lds.S` 模板、应用 crate
  和磁盘镜像；
- 写入 Cargo target 目录、生成配置和 bundle，并从有效 bundle 刷新仓库
  根目录的 `xkernel_<platform>.*` 兼容副本；
- 将 `--` 后参数传递给 QEMU。
- 将 `xkmake doc --` 后参数逐项传递给 `cargo doc`。

DWARF 段嵌入（原 `dwarf_embed` 工具）现已在 xkmake 进程内完成，
不再作为外部子进程调用；其 ELF 改写逻辑全程以 `Result` 传播并带边界
校验，但若该逻辑出错会直接影响 `xkmake build` 进程而非被隔离。

镜像元数据 finalize 同样在进程内完成。写入前必须验证 note header、owner、
descriptor 大小、文件范围及 section 同时落在 `PT_LOAD` 的文件和虚拟地址
范围内。XKMake 不在链接后新增 program segment，只修改 linker 预留字节。

所有外部命令通过 `std::process::Command` 构造，不拼接 shell 命令。
QEMU vsock 能力通过只读的 `-device help` 子进程探测；探测失败只会禁用
vsock 并产生 warning，不会阻止无 vsock 的虚拟机启动。

LinuxBoot header 与镜像拼装在进程内完成，ELF 符号、长度、偏移和整数
转换均在写文件前校验。UEFI 镜像只写入临时 bundle 路径。OVMF 固件视为
用户选择的外部可信输入；可写 VARS 使用模板副本，XKMake 不修改原模板。

文档流程只从已解析配置快照取得 feature。附加 Cargo 参数不经 shell
求值，但可改变 Cargo 的输出位置或构建行为，属于显式高级用户能力。

依赖检查仅执行固定版本的 `cargo-shear`，并以结构化参数传入受控的
workspace 路径。仅对 `Cargo.lock` 被 Git 跟踪的 workspace（内核根
workspace）传递 `--locked` 以禁止分析过程改写 lockfile；其余工具
workspace 的 lockfile 被 gitignore，由 cargo 按需生成。`--fix` 是用户
显式选择的仓库写操作，会修改被判定为无用依赖的 Cargo manifest。

Header 检查通过 Git 取得已跟踪及未忽略的 Rust 文件，只把仍存在的相对
路径分批传给固定版本的 licensure。检查模式只读；`--fix` 是显式写操作，
会依据仓库根 `.licensure.yml` 更新源文件开头，并在写入后强制复查。

覆盖率流程只在 unittest QEMU 正常退出后读取用户指定的磁盘镜像。
`debugfs` 请求中的 guest 与 host 路径会按其命令词法转义；报告命令均以
结构化参数执行，LCOV 到 Cobertura 的转换在 XKMake 进程内完成。

## unsafe 代码清单

本 crate 不包含 `unsafe` 代码。

## 可靠性不变量

- `BuildContext` 构造完成后，所有阶段使用同一份架构、平台、target 和
  profile。
- linker script 仅从仓库根目录嵌入的 `linker.lds.S` 模板和已解析
  配置快照渲染；
  目标内容不变时不重写文件，保持 mtime 稳定和 Cargo 增量编译语义。
- ELF、BIN、LinuxBoot、UEFI 和 manifest 都先写入同目录临时路径。
- 提升产物前先移除旧 manifest，并在全部产物成功提升后最后提升新
  manifest；失败的构建不会留下新的有效 manifest。
- 普通 build 把同一 workspace 中由 XKMake 原子发布的 bundle 视为可信本地
  缓存，复用要求稳定构建来源、产物名称/大小/mtime、Cargo ELF 和 x86 启动
  输入时间全部匹配。`--no-build` 是已有制品消费边界，除上述检查外还使用
  manifest 保存的构建信息和 Build ID 重新计算完整 loadable-image hash。
  自动当前时间不参与缓存判断；显式 `KBUILD_BUILD_TIME` 或
  `SOURCE_DATE_EPOCH` 覆盖值参与匹配。
- Build ID 在 build-info 和 DWARF finalize 完成后生成，哈希覆盖全部
  `PT_LOAD` 内容并将自身 descriptor 视为零，避免循环定义。
- 根目录兼容产物只从已提交的有效 bundle 复制，并先写入同目录临时路径
  后再 rename 到最终名称；复制失败会使 build 返回错误、保留旧兼容产物，
  且不会改变 bundle manifest 的有效性。目标副本的大小和 mtime 已覆盖源
  产物时跳过复制；否则标准库优先使用平台 clone/reflink 能力，不支持时
  回退普通复制。
- 外部命令非零退出立即终止当前流程，不继续消费旧产物。

## 已知限制

- 附加 QEMU 参数可以改变虚拟机安全边界，这是显式的高级用户能力。
- tap、bridge、VFIO 等需要提权的流程尚未实现，当前不会自动调用 sudo。
- 当前 bundle hash 不覆盖工具版本；manifest format version 用于拒绝未来
  不兼容格式，但工具升级后的强制重建策略仍需完善。
- guest IP/gateway 暂时进入编译环境，可能泄露到可检查的内核二进制中。

## 审计清单

- 新增外部程序时是否仍使用结构化参数？
- 新增 bundle 字段是否进入兼容性判断或 format version？
- 新增 ELF 后处理是否仍只写 linker 预留范围，并验证 `PT_LOAD` 包含关系？
- 新增路径是否限制在预期输出目录，或由用户显式指定？
- 失败路径是否可能留下看似有效的 manifest？
- 新增运行选项是否在启动 QEMU 前完成组合校验？
