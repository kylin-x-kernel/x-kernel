# xkmake — 设计文档

## 定位

`xkmake` 是 X-Kernel 仓库内的构建与运行编排器。根 `Makefile` 只提供
稳定入口，`xkmake` 负责连接 `xconfig`、Cargo、镜像加工工具和 QEMU。

## 范围

```text
src/
├── main.rs       命令分发
├── cli.rs        强类型命令行参数
├── context.rs    最终构建上下文
├── build.rs      Cargo、镜像和 bundle
├── linker.rs     内核 linker script 渲染与稳定写入
├── coverage.rs   unittest 覆盖率提取与报告生成
├── doc.rs        workspace rustdoc 编排
├── hygiene.rs    仓库卫生工具编排
├── image_metadata.rs  内嵌构建信息与 Build ID 的 ELF finalize
├── qemu.rs       平台启动策略
├── x86.rs        LinuxBoot/UEFI 启动介质
├── process.rs    外部进程边界
├── dwarf_embed.rs 进程内 DWARF 段嵌入（KFEAT_DWARF 时启用）
├── symtab.rs     紧凑内核符号表生成（KFEAT_SYMTAB）
├── symbolize.rs  日志地址的 Host 侧符号化
└── error.rs      工具层错误模型
```

## 架构

```text
Makefile
   |
   v
xkmake ------> xconfig ------> .config / config.rs / Cargo config
   |
   +---------> Cargo --------> kernel ELF
   |                            |
   |                            +--> kernel.debug.elf（未剥离，符号化输入）
   |
   +-- 按需嵌入 DWARF（KFEAT_DWARF）/ 自举符号表（KFEAT_SYMTAB）
   |      + rust-objcopy(外部)
   |                         |
   |                         v
   +--------------------> versioned bundle
                               |  \
                               |   +--> workspace-root xkernel_* copies
                               |
                               +--> make symbolize（Host 侧还原函数/行号）
                               |
                               v
                              QEMU
```

LCOV 到 Cobertura XML 的转换是 unittest 覆盖率流程的内部阶段，不暴露
独立命令或第二套参数模型。

`xconfig::build_config::ResolvedKernelConfig` 是配置器与构建器之间的稳定
语义边界。`xkmake` 不读取 `.config` 文本，也不从生成的 TOML 反向推导
架构、平台或 feature。

## 核心流程

### Build

1. 校验当前目录是 X-Kernel workspace。
2. 通过一次 `ConfigEngine` 求值生成完整的 `ResolvedKernelConfig` 快照。
3. 从该快照生成 `.config`、`auto.conf`、`autoconf.h`、`config.rs`、
   rust-analyzer 设置和 Cargo 配置，并从仓库根目录
   `linker.lds.S` 渲染当前 target 的 linker script；生成阶段不再次
   解析 Kconfig。linker script 只在
   内容变化时重写，避免无效更新 mtime 使 Cargo 重新编译。
4. 构造一次性的 `BuildContext`，之后不再读取环境或配置文件。
5. 调用 Cargo 生成目标 ELF。
6. x86_64 额外增量构建 boot stub 与 EFI loader。
7. 若现有 bundle 的稳定构建来源、配置 hash、输入时间、产物名称、大小和
   mtime 顺序仍匹配，则作为可信本地缓存直接复用，并保留原有构建时间；
   普通 build 不重新扫描完整 ELF。
8. 否则在临时路径复制 ELF、按需嵌入 DWARF（KFEAT_DWARF），再原地填充
   linker 预留的 `.note.xkernel.build-info` 与 `.note.gnu.build-id`；自动
   构建时间在此时取当前 UTC 时间，而不是参与前置缓存判断。
9. Build ID 对 build-info 已写入、Build ID descriptor 清零后的全部
   `PT_LOAD` 内容计算 SHA-256。
10. 从 finalized ELF 生成 raw BIN。
11. x86_64 在进程内生成 LinuxBoot header/镜像，并创建 UEFI FAT 镜像。
12. 移除旧 manifest，提升所有产物，最后提升新 manifest 作为有效性标记。
13. 从有效 bundle 刷新仓库根目录的 `xkernel_<platform>.*` 兼容产物，供
    Jenkins 制品传递和仍使用旧命名的工具消费。复制先写入不存在的同目录
    临时路径，使 APFS/Linux CoW 文件系统可走 clone/reflink 快速路径，再
    以 rename 替换最终文件；文件系统不支持时由标准库回退普通复制。

### 产物

每个 bundle 包含两类 ELF：

- `kernel.elf`：启动/调试 ELF。KFEAT_DWARF 时嵌入 DWARF 可加载段，否则
  与 Cargo 产物一致；
- `kernel.debug.elf`：**始终**是未加工的 Cargo 产物（非 alloc `.debug_*`
  完整保留），是 `xkmake symbolize` 的符号化输入。

### 符号表自举（KFEAT_SYMTAB）

紧凑符号表（kallsyms 风格）来自内核自身符号，因此存在"表需要链接产物、
产物需要表"的循环。构建按两轮收敛：

1. 首轮以空表链接；`create_bundle` 从 Cargo ELF 提取函数符号并写入
   `$TARGET_DIR/kbuild/ksymtab.bin`；
2. 内容变化时再次调用 Cargo 重链（`util/backtrace` 通过 build.rs 复制
   该 blob 并以 `include_bytes!` 嵌入），随后重建 bundle。

收敛性由布局保证：blob 位于 `.rodata`（普通 static），符号表只收录
`.text` 系段的函数，而 `.text` 在 `.rodata` 之前——blob 大小的变化
不会改变表内记录的地址。第二轮的 blob 与第一轮相同，构建停止。

KFEAT_SYMTAB=n 时同样会落盘一个空的 16 字节 blob。`util/backtrace`
的 build.rs 以 `cargo:rerun-if-changed` 跟踪 `$TARGET_DIR/kbuild/
ksymtab.bin`，而 Cargo 对不存在的 `rerun-if-changed` 输入一律视为
dirty，会导致 backtrace 及其全部传递依赖在每次构建时被全量重编译。
保持该输入始终存在且内容稳定，才能让后续构建完整命中 Cargo 增量缓存。

### Symbolize

`make symbolize LOG=<log>` 从日志提取 `Backtrace:` 块的原始地址（兼容
`func+0xoff/0xsize` 注解），调用 `llvm-addr2line`/`addr2line` 对照
`kernel.debug.elf` 输出函数名与文件/行号。`--offset` 支持内核加载地址
偏移（如 KASLR）。

`make run`/`make justrun` 自动符号化：QEMU 输出实时 tee 到终端并镜像到
`bundle/qemu.log`，运行结束后扫描 backtrace 帧，发现则自动对照
`kernel.debug.elf` 解析并打印（`--no-symbolize` 关闭）。tee 由
`Process::run_tee` 实现：stdout 走管道由读取线程转发到终端与日志文件，
stdin/stderr 保持继承，交互会话不受影响。

### Run

1. 执行 Build 流程并取得 bundle。
2. 根据已解析平台选择封闭的 QEMU 启动策略。
3. 根据编译进内核的驱动和 CLI 选项添加设备。
4. 以结构化参数启动 QEMU，并传播退出状态。
5. unittest QEMU 成功退出后，从磁盘镜像提取 `default.profraw`，依次生成
   `default.profdata`、`coverage.txt`、`coverage.info` 和 `coverage.xml`。

覆盖率产物位于 `target/<rust-target>/<profile>/`。开始生成前会删除旧
产物，任一步失败都会使本次 run 失败，避免 CI 消费过期报告。

### Doc

1. 与 build 流程相同，通过一次 `ConfigEngine` 求值得到配置快照。
2. 从快照生成构建配置和 linker script，并直接取得
   Cargo feature 集。
3. 使用解析后的内核 Rust target 调用 `cargo doc --workspace --no-deps`，
   不在 host target 上解释内核 ABI，也不解析生成的 Cargo TOML。
4. `--check-missing` 在保留 broken-link/cfg 检查的基础上拒绝缺失 rustdoc。

### Hygiene Deps

1. 校验 PATH 中的 `cargo-shear` 版本与 XKMake 固定版本一致；
   工具缺失或版本不符时引导执行 `make install-tools`。
2. 对根 workspace、`xtask`、`tee_apps` 和 `uapps/hello` 依次执行检查。
3. 仅对 `Cargo.lock` 被 Git 跟踪的 workspace（内核根 workspace）传递
   `--locked`，防止卫生检查隐式改写已提交的 lockfile；`xtask`、
   `tee_apps`、`uapps/hello` 的 lockfile 被 gitignore，没有已提交锁，
   不传 `--locked`，由 cargo 按需生成。
4. `--fix` 仅透传 cargo-shear 的依赖删除能力；warning 保持提示语义，
   error 或任何 workspace 检查失败都会终止命令。

### Hygiene Header

1. 校验 PATH 中的 `licensure` 版本与 XKMake 固定版本一致；工具缺失或
   版本不符时引导执行 `make install-tools`。
2. 通过 Git 获取已跟踪及未忽略的未跟踪 Rust 源文件，并过滤工作树中已
   删除的路径；非 Rust 文件不进入 header 检查。
3. 分批将文件传给 licensure，避免仓库增长后触及进程参数长度限制。
4. 普通模式执行 `--check`；`--fix` 先执行 `--in-place`，再以 `--check`
   复查最终状态。header 模板和注释格式由仓库根 `.licensure.yml` 定义。

x86_64 bundle 同时携带 `kernel.bzimg` 和 `kernel.uefi.img`。默认使用
LinuxBoot，`--boot uefi` 使用 OVMF。OVMF VARS 模板在每次运行时复制到
`target/xkmake/runtime/<platform>/`，不会修改固件模板或 bundle。

vsock 采用能力探测：内核启用 virtio-socket 且用户未传 `--no-vsock`
时，XKMake 查询当前 QEMU 的设备列表。仅在对应 `vhost-vsock-*` 型号
存在时添加设备；不支持时输出 warning 并继续启动。

网络转发端口与 vsock CID 采用顺序探测。`--hostfwd-port`（默认 61005）
是转发到 guest TCP/UDP 5555 的首选主机端口，被占用时按 10 为步长
（61005、61015、...、62005）在 `61005..=62005` 内查找首个 TCP 与 UDP
均空闲的端口；探测区间耗尽时报错退出。端口探测只把 `AddrInUse` 视为
占用，其他 bind 错误（如非 root 绑定 <1024 端口）直接报错，不会静默
换端口。`--vsock-cid`（默认 103）是首选 guest CID，被占用时在
`103..=203` 内逐个顺序查找空闲 CID。CID 探测通过临时打开
`/dev/vhost-vsock` 并执行 `VHOST_VSOCK_SET_GUEST_CID` 完成，内核以
`EADDRINUSE` 表示 CID 已被占用，探测 fd 随即关闭释放 CID；设备缺失或
无权限探测时退回首选值，避免探测失败阻止虚拟机启动，而内核 `EINVAL`
（保留 CID）直接报错；`--vsock-cid` 在参数解析时校验不小于 3（内核
保留 0/1/2）。`--dry-run` 不执行任何探测，直接使用首选端口与 CID 构造
命令行。

`--no-build` 会只接受 manifest、构建信息、内嵌 Build ID、产物和 Cargo ELF
时间均匹配的现有 bundle。复用校验会重新计算 ELF 的 loadable-image hash，
避免消费被修改或不完整的 bundle。`config` 子命令为 Makefile 工具目标提供只读的架构、
target、平台和 bundle 路径查询，避免 Makefile 重新解析 `.config`。

## 设计决策

- Cargo 负责源码增量编译，bundle 只缓存镜像加工结果。
- 普通 build 的复用属于可信本地缓存边界，依靠 manifest 最后提交的不变量、
  产物大小与 mtime 快速判断；`--no-build` 属于已有制品消费边界，额外执行
  完整 loadable-image SHA-256 校验。
- 每次 build 仍调用 Cargo 检查源码新鲜度；源码和配置未变化时 Cargo
  增量流程不重新编译，XKMake 也不重新生成 ELF metadata、BIN 或启动镜像。
  KFEAT_SYMTAB 的符号表自举在 blob 内容不变时不触发第二次链接。
- 构建来源元数据不进入 Cargo 编译环境；linker 预留固定 note，XKMake 在
  bundle 阶段原地写入，因此机器名和时间变化不会让 kernel crate 重编译。
- 自动构建时间表示 bundle 本次真正生成的 UTC 时间，不是缓存检查输入；
  `KBUILD_BUILD_TIME` 或 `SOURCE_DATE_EPOCH` 是显式覆盖值，值发生变化时会
  使已有 bundle 失效。
- 镜像元数据 ABI 由 `boot/kernel-image-metadata` 持有；XKMake 负责收集，
  entry 只负责校验后的展示。
- linker script 是配置派生的 host 编排产物，由 XKMake 在调用
  Cargo 前生成；`kernel-boot` 不再用 `build.rs` 生成它。
- `.config` 仍是内核产品配置的唯一事实来源。
- QEMU 参数使用 `Vec<OsString>` 语义逐项传递，不经过 shell 求值。
- `xkmake` 仅依赖 `xconfig::build_config` 门面，不依赖 CLI 实现模块。
- 没有 `.config` 时不猜测默认平台。
- bundle 是规范产物位置；仓库根目录的 `xkernel_<platform>.*` 是从有效
  bundle 刷新的兼容副本，不参与 bundle 有效性判断。
- LinuxBoot 镜像布局由 XKMake 的 Rust 代码生成，不依赖 Python 脚本。
- rustdoc feature 与内核构建使用同一份解析快照，不维护第二套配置读取器。
- 依赖分析使用固定版本的外部 cargo-shear，不在 XKMake 内复制 Cargo/Rust
  解析逻辑，也不把其大型依赖树链接进 XKMake。
- Rust 源文件 header 由固定版本的外部 licensure 按仓库配置检查和修复；
  XKMake 只负责确定文件集合、分批调用和传播失败状态。
- guest IP 和 gateway 当前属于构建输入，因为现有网络栈通过
  `option_env!` 编译它们；后续应迁移为内核命令行参数。

## 当前限制

- AArch64、RISC-V 64、LoongArch64 QEMU virt 使用 raw BIN 直接启动。
- x86_64 LinuxBoot 和 UEFI 已支持；其他 x86 平台仍需独立启动策略。
- unittest 已生成 LLVM 覆盖率报告；更丰富的测试结果聚合尚未迁移。
