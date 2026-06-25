# kexec — 设计文档

## 定位

`process/kexec` 负责用户程序镜像装载和 exec 初始地址空间布置。

它是内存管理子系统的 client，而不是 memory-management owner。它解析 ELF，
决定需要哪些用户映射，然后通过 `mm/memspace` 和 `mm/filemap` 的正式
接口安装这些映射。

## 背景

ELF `PT_LOAD` 段需要保留 Linux 风格的 file-private mapping 语义：

- VMA 起点和 file offset 都按页向下对齐；
- 首次缺页从可执行文件内容读取；
- 写入私有映射后进入 anonymous COW 结果页；
- BSS / `memsz > filesz` 的尾部按映射规则零填充；
- loader 不应自己维护 file object identity、COW page owner 或 VMA runtime。

这些职责已经归属到 MM 组件：

- `mm/memspace` 拥有 `MmSpace`、VMA 集合、页表协调和映射安装；
- `mm/filemap` 是 file-backed VMA/runtime 装配 adapter；
- `mm/pagecache` 拥有 file-backed cached content；
- `mm/anon` 拥有 private anonymous / COW result pages。

## 范围

相关源码：

```text
process/kexec/src/lib.rs
process/kexec/src/loader.rs
process/kexec/src/lru_cache.rs
```

本 crate 不包含：

- VMA tree；
- page table mutation；
- file-backed object identity；
- private COW page ownership；
- syscall ABI validation。

## 架构

```text
ExecRequest
  -> ExecSource::Path | ExecSource::Resolved
  -> ExecRequest::prepare()
       -> resolve executable Location through kvfs::namei LookupIntent::Exec
       -> open and pin executable kfs::File
       -> build BinPrm

load_user_app()
  -> ExecRequest::from_path()
  -> internal request loader

load_user_app_request()
  -> caller-provided ExecRequest
  -> ExecRequest::prepare()
  -> ElfLoader::prepare_binprm()
       -> load/cache ELF headers and program headers
       -> resolve shebang recursion if needed
       -> resolve and pin optional PT_INTERP object
  -> ElfLoader::commit_prepared_binprm()
       -> MmSpace::clear()
       -> ksignal::map_signal_trampoline()
       -> map_elf() for executable and optional interpreter
            -> filemap::new_file_private_vma()
            -> MmSpace::map_runtime_vma()
       -> build aux vector
```

`ExecRequest::prepare()` is the pre-replacement phase. It may fail without
modifying the old address space because it only resolves/pins the executable
and owns argv/env strings. `ElfLoader::prepare_binprm()` extends that
pre-commit phase by validating the executable shape, handling shebang
redirection, and pinning the optional interpreter object. `ElfLoader::commit_prepared_binprm()`
is the address-space replacement phase: after `MmSpace::clear()` succeeds,
callers must not run fallible metadata work that would return control to the
old user image.

The component boundary is:

```text
process/kexec
  parses image and creates mapping requests
      |
      v
mm/filemap
  builds file-private VMA metadata and runtime
      |
      v
mm/memspace
  owns insertion into the address space and page-table coordination
```

## 调用约束 / 执行上下文

- 运行在有 current process / current filesystem context 的普通进程上下文。
- 可能读取文件、分配内存、清空并重建用户地址空间，因此允许睡眠。
- 不适用于中断上下文。
- 装载过程中持有的 `MmSpace` 是即将执行的新用户地址空间。

## 算法流程

### exec request / binprm

`ExecRequest` 是 exec 调用进入 loader 前的 owned request：

- `ExecSource::Path(String)` 表示由当前进程文件系统上下文解析路径；
- `ExecSource::Resolved { location, display_path }` 表示调用方已经通过
  VFS/namei 得到可执行节点，例如 `/proc/self/fd/N` magic link 的目标，并保留
  Linux `bprm->filename` 风格的用户显示路径；
- `args` 和 `envs` 在 request 内部拥有，避免依赖用户缓冲区生命周期。

`ExecRequest::prepare()` 只做 executable resolution 和 executable `File` pinning，
生成 `BinPrm`：

- `BinPrm::location()` 是已固定的可执行 VFS location；
- `BinPrm::executable()` 是对应已打开 executable file；
- `BinPrm::display_path()` 用于 argv/script reconstruction；
- `BinPrm::args()` / `envs()` 是 owned exec 参数。

该阶段不清空或修改目标 `MmSpace`。

`load_user_app()` 保留路径字符串入口，适用于仍以路径字符串发起 exec 的调用方，
内部也使用 `LookupIntent::Exec`。`load_user_app_request()` 接收完整
`ExecRequest`，适用于 syscall 层已经完成 `LookupIntent::Exec` namei 策略的入口，
包括 procfd magic-link、内核初始化路径、后续 `fexecve`/`AT_EMPTY_PATH` 以及其它
open-executable 来源。

对于 `ExecSource::Resolved`，调用方必须同时传入用户显示路径；loader 不重新解释
procfs 路径字符串，也不单独实现 magic-link 修正。

### ELF 头缓存

1. `BinPrm` 提供已固定 executable `File`。
2. 读取 ELF header 和 program header。
3. 使用小型 LRU 避免重复解析最近装载的镜像。

### `PT_LOAD` 映射

1. 遍历 ELF `PT_LOAD` program header。
2. 计算用户虚拟地址、页内偏移、页对齐 VMA 起点和页对齐 file offset。
3. 将 ELF flags 转成 `MappingFlags`。
4. 调用 `filemap::new_file_private_vma()` 构造 file-private VMA
   metadata 与 runtime。
5. 调用 `MmSpace::map_runtime_vma()` 安装到地址空间。

这里 `process/kexec` 不直接构造 `VmArea` 的 file metadata，也不直接持有
`VmRuntimeRef` 的内部实现。

### 动态链接器

如果 ELF 带 `PT_INTERP`：

1. 从主可执行文件读取 interpreter 路径；
2. 以 `LookupIntent::Exec` 解析并缓存动态链接器 ELF；
3. 在 `USER_INTERP_BASE` 处通过同一套 `map_elf()` 路径映射。

### 脚本解释器

如果 executable 不是 ELF 且文件头以 `#!` 开始：

1. 读取首行 shebang；
2. 将解释器路径和可选解释器参数放到新 argv 前缀；
3. 将脚本显示路径作为解释器 argv 的下一个参数；
4. 保留原 argv 中除 `argv[0]` 以外的 tail；
5. 递归进入同一套 `ExecRequest` / `BinPrm` / loader 流程。

脚本递归深度由固定上限控制，超过上限返回 loop 类错误。

## 并发模型

- `ElfLoader` 内部的 LRU cache 由外部静态 `Mutex` 序列化。
- 单次 `load_user_app()` / `load_user_app_request()` 对传入 `MmSpace` 做独占修改。
- 本 crate 不维护跨进程共享的 VMA 或 page-table 状态。

## 设计决策

1. `process/kexec` 只作为 MM client。
   原因：ELF loader 需要知道 image layout，但不应拥有 VMA/object/page-table
   生命周期。

2. file-private `PT_LOAD` 映射统一走 `filemap` adapter。
   原因：file-backed VMA 的 `vm_pgoff`、file object identity、EOF/BSS 语义和
   COW 目的对象必须和 `mmap(MAP_PRIVATE)` 使用同一组 MM 组件边界。

3. stack/heap 不在 kexec 内部形成独立 memory subsystem。
   原因：它们应通过 `mm/memspace` 的 anonymous-private 映射接口进入普通 VMA
   与 page-fault 主线。

4. exec 后处理不在 point-of-no-return 之后返回普通错误。
   原因：`MmSpace::clear()` 已销毁旧用户镜像；装载成功后的 task name、exe path、
   TEE metadata 和 fd cleanup 必须使用预先解析的数据或 best-effort 更新，不能再
   把错误返回到旧用户态。

## Drop / 资源释放

- ELF header/cache 数据跟随 `ElfCacheEntry` 和 LRU cache 生命周期释放。
- 用户地址空间资源由 `MmSpace` 拥有，`process/kexec` 不在 drop 路径中释放
  VMA、page table 或 anonymous/file-backed object。
