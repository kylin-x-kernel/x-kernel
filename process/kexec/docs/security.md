# kexec — 安全与可靠性分析

## 信任模型

`process/kexec` 信任内核 MM 组件维护 VMA、page table、file object identity 和
COW 生命周期。它不信任用户提供的可执行文件内容、interpreter 路径或 ELF
metadata。

调用方必须提供可修改的 `MmSpace`。文件路径解析依赖当前进程文件系统上下文，
并必须通过 `kvfs::namei` 的 `LookupIntent::Exec` 规则完成。如果调用方已经完成
VFS/namei 解析，可以通过 `ExecSource::Resolved { location, display_path }` 和
`load_user_app_request()` 传入已解析 `Location` 与用户显示路径，避免 loader
重新解释路径字符串。

## 外部边界 / 攻击面

- 可执行文件内容来自文件系统，可能格式错误或恶意构造。
- `ExecSource::Path` 来自 syscall path 字符串，解析依赖当前 fs context 和
  `LookupIntent::Exec`。
- `ExecSource::Resolved` 来自上游 VFS/namei 结果，loader 信任其已经完成 exec
  路径策略，并保留调用方提供的 display path。
- `/proc/self/fd/N` 这类 procfs magic-link exec 必须在 syscall/namei 层通过
  `LookupIntent::Exec` follow 成 `Location` 后传入 loader，不能在 loader 内部
  解析 procfs 字符串。
- ELF program headers 控制映射地址、大小、权限和 file offset。
- `PT_INTERP` 内容控制动态链接器路径。
- `#!` 脚本首行控制解释器路径、可选解释器参数和递归装载流程。
- 文件读取可能失败、返回短读或触发底层文件系统错误。
- 地址空间重建会影响即将运行的用户进程。

本 crate 不直接处理用户指针、MMIO、DMA、FFI、inline assembly 或硬件寄存器。

## unsafe 代码清单

当前 `process/kexec` 源码不包含显式 `unsafe` 块。

## 内存安全不变量

- ELF header 和 program header 解析必须在 owned buffer 上完成，不能保留指向
  临时栈数据的引用。
- `ExecRequest` 必须拥有 argv/env 字符串，不能保存用户指针或临时借用。
- `BinPrm` 必须持有 executable `File`，保证准备完成后 executable object
  在 loader 消费期间仍然有效。
- `ExecRequest::prepare()` 不得清空或修改目标 `MmSpace`。
- shebang 解释器重写和 `PT_INTERP` 路径解析必须发生在 point-of-no-return
  之前，避免普通解析失败落在旧镜像已销毁之后。
- `ExecSource::Resolved` 不能重新按字符串路径查找 executable，否则会丢失
  procfs magic-link 的对象引用语义。
- `ExecSource::Resolved` 的 display path 只能用于进程元数据和脚本 argv
  reconstruction，不能作为重新 lookup 的 authority。
- `MmSpace::clear()` 是 exec 的 point-of-no-return：清空后不能再执行会把普通
  错误返回到旧用户镜像的 metadata 更新。
- `PT_LOAD` 映射必须使用页对齐 VMA 起点和页对齐 file offset，避免 file prefix
  被错误当作匿名零填充。
- loader 不得绕过 `mm/filemap` 直接构造 file-backed VMA metadata。
- loader 不得绕过 `MmSpace::map_runtime_vma()` 直接修改页表。
- `MmSpace::clear()` 后必须重新安装用户运行所需的 trampoline 和 ELF mappings。

## 线程安全

- 全局 ELF loader 由 `Mutex` 保护。
- 单次装载期间对目标 `MmSpace` 的修改由调用方以可变引用形式传入。
- LRU cache 中的 executable `File` 引用共享 inode-owned page cache，但不拥有
  MM 映射状态。

## 威胁分析

- 恶意 ELF header 可能声明越界 program header。
  缓解：ELF parser 验证 header 范围；失败返回 executable invalid 错误。
- 恶意 `PT_LOAD` 可能请求异常大的映射。
  缓解：映射安装交给 `MmSpace`，由地址空间和页表层返回错误。
- 错误的 file offset 对齐会破坏 private executable mapping 语义。
  缓解：loader 对 `p_vaddr` 与 `p_offset` 的页内偏移关系做断言，并把
  对齐后的 offset 传给 `filemap`。
- loader 如果直接构造 VMA/file metadata，会绕开 file-backed mmap 的统一对象
  语义。
  缓解：当前只调用 `new_file_private_vma()` 和
  `MmSpace::map_runtime_vma()`。

## 故障模式与影响分析（FMEA）

| 故障 | 触发条件 | 局部影响 | 系统影响 | 缓解 |
| --- | --- | --- | --- | --- |
| ELF 解析失败 | header 无效或 program header 不完整 | exec 失败 | 当前进程无法装载目标程序 | 返回 `InvalidExecutable` / `InvalidData` |
| executable prepare 失败 | path 无法解析或 executable file 无法打开 | exec 准备失败 | 旧进程镜像不应改变 | `ExecRequest::prepare()` 返回错误且不接收 `MmSpace` |
| interpreter 路径无效 | `PT_INTERP` 不是有效 C string 或路径不可解析 | 动态链接程序装载失败 | exec 失败 | 返回 `InvalidInput` 或文件系统错误 |
| script recursion loop | shebang 解释器链循环或过深 | exec 失败 | 当前进程无法装载目标程序 | 固定递归上限，超过后返回 loop 类错误 |
| 映射安装失败 | 地址冲突、OOM、页表错误 | 部分地址空间构造失败 | exec 失败，调用方处理错误 | MM API 返回 `KResult` |
| point-of-no-return 后元数据更新失败 | 地址空间已清空后继续执行可失败路径 | 旧用户镜像不可恢复 | 不能把错误返回旧用户态 | 可失败解析前移；后处理使用预解析数据或 best-effort |
| cache 内容过期 | 底层文件变化但 LRU 仍持有旧 header | 可能使用旧解析结果 | 可执行文件更新可见性延迟 | 当前 ELF cache 未接入文件失效通知 |

## 故障管理

- 普通失败通过 `KResult` 返回。
- ELF 解析错误统一转换成 executable invalid 类错误。
- 目前存在少量 `assert_eq!` 用于校验 ELF alignment 和读长度假设；这些路径表示
  loader 内部一致性假设，触发时会导致 panic 而不是普通 exec 错误。

## 隐私分析

`process/kexec` 会读取用户程序文件内容和 interpreter 路径，但不记录文件内容。
调试日志只输出路径或映射区间等元数据。

## 已知限制

- `process/kexec/docs/design.md` 和本文件只描述当前 loader 与 MM 的 client
  边界，不设计完整 exec 凭据、权限或命名空间语义。
- ELF cache 当前未与文件失效通知同步。
- 完整 `mmap`/`mprotect`/COW 语义由 MM 组件保证，不在本 crate 内验证。

## 审计清单

- `process/kexec` 是否仍只通过 approved MM APIs 建立 file-private mappings？
- 是否有代码直接构造 file-backed `VmArea` 或直接修改 page table？
- `PT_LOAD` 的 VMA start 和 file offset 是否同时按页向下对齐？
- `PT_INTERP` 路径读取是否处理了无效字符串和文件系统错误？
- 新增 loader 逻辑是否仍保持 `process/kexec` 作为 MM client，而不是 MM owner？
