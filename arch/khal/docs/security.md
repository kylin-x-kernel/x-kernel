# khal — Security and reliability

This document describes the IRQ adapter trust model and page-table storage and
mapping-reference boundaries separately. Basic tests do not replace hardware
retirement protocol verification.

`nmi::shutdown_periodic_nmi` forwards the local platform result unchanged. A
false result cannot authorize source reuse or handler destruction. The caller
owns restart exclusion and completed shutdown on all participating CPUs; this
facade does not supply a remote execution or hardware-drain barrier.

`pmu::shutdown_overflow_irq` and `restore_overflow_irq` forward source-owner
errors unchanged. Their provider validates sleepable context and retains failed
shutdown state. Callers must authorize service loss and exclude system stop and
CPU hotplug; success is not permission to restart old watchdog consumer state.

## 信任模型

`khal::irq` 是 trap-entry adapter，不拥有 generic IRQ state，也不 re-export
generic `kirq` API。
它信任：

- `kcpu` trap dispatch 只在正确异常上下文调用 `irq_handler()` / `nmi_handler()`；
- `kspin::NoPreempt` 能覆盖进入 `kirq` 的 adapter 调用区间；
- `kirq` owns IRQ state, Line-owned NMI actions, dispatch and completion invariants.
- `kplat::nm_irq` 的平台实现（`NmiMechanismIf` / `PeriodicNmiSourceIf`）与 `karch` 的 NMI 寄存器
  访问（PMR / ALLINT / SCTLR_EL1.NMI）正确反映机制与降级状态。

## 外部边界 / 攻击面

1. **trap dispatch 边界**
   `kcpu` 通过 registered trap handler 进入本模块。错误 trap 分类会把 normal IRQ
   与 pseudo-NMI 语义混淆。

2. **NMI 机制 facade 边界**
   `khal::nmi` 把 `mode()` / `enable_periodic_nmi()` 等接口 re-export 给消费者。
   机制不可用（`mode() == NmiMode::None`）时，消费者必须禁用依赖 NMI 的功能；
   `configure_nmi` can also reject Closing, unsupported source types or failed
   local preparation. A false result must not be treated as prepared delivery;
   partial controller failure retains cleanup obligations in KIRQ. Periodic
   source activation remains separate from this preparation contract.
   最终停机路径经 `quiesce_nmi()` 停掉当前 CPU 的周期源；该路径持有
   `NoPreempt`，并依赖 PMU backend 的共享计数器访问与原子 enabled 状态来安全
   覆盖可能已经到达的 NMI 分发。

3. **Firmware / device-tree handoff boundary** (`khal::firmware`)

   `khal::firmware` re-exports the boot-loader handoff data: `cmdline`, the
   validated device-tree blob (`dtb_bytes`) and `dt_log_level`. The last one
   receives external firmware data directly — the value of
   `/chosen/x-kernel,log-level`, read through `of::chosen_log_level()` — and
   returns it unchanged:

   - data direction: bootloader/board device tree -> `of` -> `khal::firmware` ->
     `kruntime::resolve_boot_log_filter`;
   - trust assumption: the device tree is supplied by the boot chain, so it is
     semi-trusted; a wrong value must not be able to disable logging;
   - validation responsibility: this layer deliberately does **not** parse the
     value. `klogger::set_filter_spec` owns the level vocabulary and rejects a
     malformed spec, and `kruntime` then falls through to the command line and
     the Kconfig default after printing the rejection through `kprintln!` (the
     unfiltered console path, since the level being installed is what would filter
     a `warn!`), so a typo cannot silence the boot log;
   - failure handling: the accessor returns `Option<&'static str>`, so a missing
     property and a property whose bytes are not valid ASCII/UTF-8 both degrade
     to "no board preference" instead of a partially applied level.

   `khal::firmware` only carries that handoff data; apart from these values the
   module still handles no user pointers, DMA buffers, device MMIO or
   file/network data, and it performs no semantic validation of the log level.
   The exposure is bounded by the consumer: the string is passed straight to
   `klogger::set_filter_spec`, which validates the whole spec and keeps the
   previous filter when it does not parse, so a hostile or truncated property can
   change verbosity but cannot corrupt kernel state or silence logging
   permanently.

4. **Firmware-node publication boundary** (`khal::firmware::fwnode`)

   `fwnode_root()` exposes the validated device tree through the neutral
   `fwnode` contract. The root handle is a borrowed `FwNode<'static>` over
   `rs_fdtree::DtBackend`; it contains no raw pointer and no mutable state.
   Publication is allocation-free. The first indexed node lookup builds a
   `NodeIndex`, which allocates `Vec` metadata after the allocator exists;
   `for_each_compatible` is consequently restricted to post-allocator boot
   stages, while early console/PMU users stay on the allocation-free
   `FdtNode` accessors.

   Node iteration is bounded by the already-validated structure block. A
   malformed availability declaration, dangling phandle, bad `#interrupt-cells`
   or untranslatable address encoding degrades to `None`/`FwError` for that
   node rather than panicking or indexing firmware-controlled offsets.
   `for_each_compatible` decodes the primary MMIO resource through translated
   `ranges`; a tree without an ancestor `ranges` therefore reports no MMIO
   instead of adopting an untranslated address. It exposes the IRQ declaration
   as `Result<Option<FwIrqSpec>, FwError>`, so an unreadable or malformed
   declaration remains an error and is never conflated with an absent IRQ or a
   raw firmware number. Resolved DMA facts are not snapshotted here: the
   neutral `FwNode` is retained, and consumers query `dma_window()` /
   `dma_effective_coherency()` on demand, so the backend performs the
   firmware-specific DMA-parent walk, ancestor translation and coherency
   inheritance. Absence crosses the contract as `Ok(None)`, while malformed or
   unsupported descriptions stay `Result` errors; this layer substitutes no
   unlimited mask, generic coherency default or ancestor translation. The
   handle grants no memory or hardware access. This layer adds no cell decode,
   unsafe code, mapping or hardware access.

本模块不直接处理用户指针、DMA buffer、设备 MMIO 或文件/网络数据。

`arch/khal/src/irq` 当前没有本地 `unsafe` 代码块。相关 unsafe 边界位于：

- `kcpu` trap/汇编入口；
- 平台中断控制器 backend 的 MMIO/PIO/汇编屏障；
- `kiface` generated linkage。

## 内存安全不变量

- `khal::irq` owns no IRQ registry or NMI handler storage, avoiding duplicate
  active state beside `kirq`.
- `NoPreempt` guard 必须覆盖整个 `kirq::handle_irq()` / `kirq::handle_nmi()` 调用。
- `kiface_task_api::TaskIrqExitCheckpointIf` (defined by kiface-task-api, provided by ktask)
  is reserved for the completed normal IRQ tail, after controller
  completion and dispatch cleanup. It may exempt only the entry IRQ mask from
  ordinary preemption checks, never outer preemption guards or NMI context.
  The adapter restores the suspended exception marker after the checkpoint.

## 线程安全

`khal::irq` adapter 不引入新的共享 mutable state。并发控制主要由：

- `NoPreempt` 保护 trap adapter 执行区间；
- `kirq` 内部 lock 模型保护 IRQ core state；
- `kiface` 单实现接口连接 platform/backend provider。

## 威胁分析

| 编号 | 威胁 | 触发条件 | 影响 | 缓解 |
|------|------|----------|------|------|
| T-01 | trap adapter 未禁用抢占 | handler 直接调用 `kirq` 而无 `NoPreempt` | hardirq exit 后调度边界错误 | `irq_handler()` / `nmi_handler()` 创建 `NoPreempt` |
| T-02 | generic IRQ API 重新经由 `khal::irq` 导出 | 调用方把 HAL adapter 当公共 IRQ core | crate 边界退化，后续扩展回到 HAL | `khal::irq` 不 re-export `kirq::*`，调用方直接依赖 `kirq` |
| T-03 | `IPI_IRQ` 移入 `kirq` | generic core 依赖 `kbuild_config` | crate 边界污染、特性耦合 | IPI 调用方直接使用 `kbuild_config::IPI_IRQ` |
| T-04 | x86 helper 回流到 `khal::irq` | HAL adapter 再次暴露 APIC vector policy | crate 边界退化，驱动绕过 `kirq` MSI resource | `khal::irq` 不提供 MSI-X/APIC helper；x86 APIC 只实现 `kirq::MsiBackendIf` |
| T-05 | 降级构建仍调用 NMI 接口 | 平台无 FEAT_NMI / GICv2 时消费者未检查 `mode()` | 无效寄存器访问或误报 NMI 已启用 | `mode() == None` 时平台返回 `false` / watchdog 启动期禁用 hardlockup；调用者契约要求先查询 `mode()` |

## 故障模式与影响分析（FMEA）

| 编号 | 故障模式 | 局部影响 | 系统影响 | 检测/缓解 |
|------|----------|----------|----------|-----------|
| F-01 | `kirq::handle_irq()` 返回前 `NoPreempt` 过早释放 | lifecycle/dispatch 上下文错误 | 调度状态异常 | adapter guard 生命周期覆盖整个调用 |
| F-02 | 调用方仍使用 `khal::irq` generic API | 编译失败 | 暴露未迁移调用点 | `rg "khal::irq"` audit + Make/Kconfig build |
| F-03 | trap handler 未链接 | CPU IRQ/NMI 无法进入 `kirq` | 平台无法处理中断 | platform build 覆盖和 trap registration 检查 |

## 故障管理

`khal::irq` adapter 本身不处理 recoverable IRQ 错误；`kirq` 负责注册失败、
unknown IRQ/NMI warning 和 completion 管理。adapter 返回 `kirq` handler 的布尔结果。

## 隐私分析

`khal::irq` 不处理用户数据。它只处理 trap vector。

## 已知限制

- The IRQ analysis covers `src/irq/`; the generic IRQ core threat model belongs
  to `arch/kirq/docs/security.md`. This document also covers the page-table
  storage and mapping-reference adapters in `src/paging.rs` and the time
  boundaries listed below.
  Those adapter contracts do not establish hardware quiescence or complete
  system-wide retirement protocols.

## Crate-level test boundaries

The original eight guest cases under ut-tests/{percpu,paging,mem,time}.rs retain
their module registration. Nine new time cases use isolated epoch/deadline
fixtures. Together with seven page-reference cases, there are 24 cases including
ut-tests/time_epoch.rs and ut-tests/paging_page_refs.rs. The huge-leaf case is
serial and retains no test-owned backing reference across unmap; allocation
accounting verifies that both exact and compound backing survives until
`try_finish()` succeeds.
`inactive_table_destruction_releases_its_live_data_hold` leaves a live managed
leaf in a never-activated tree and checks that destruction returns its real
PageRef to allocator inventory. The time fixtures contain no handwritten unsafe
code. The page-storage transfer test
uses `unsafe { PageRef::from_raw(pa, 1) }` for its exact outstanding transfer; the
adjacent SAFETY comment documents ownership and the absence of hardware use.
These tests do not establish IRQ/NMI concurrency or real platform suspend
safety. Test mounts remain gated by cfg(unittest).

## 审计清单

- `khal::irq` 是否仍只作为 adapter/facade，不新增 IRQ state。
- trap handler 是否在调用 `kirq` 前创建 `NoPreempt`。
- `khal::irq` 是否没有重新导出 generic `kirq` API。
- x86 APIC helper 是否仍未回流到 `khal::irq`。

## Monotonic suspend/resume boundaries

`time/epoch.rs` and `time/suspend.rs` contain no handwritten unsafe or user/device
memory access. They trust backend sampling, stable frequency and PM quiescence.
Atomic sequence validation prevents mixed epoch snapshots. Identities do not
repeat; suspend reserves a resume identity, and rejected requests preserve the
active mapping. Monotonic and ktime accounting tokens are independent: neither
this layer nor its token proves that a real sleep occurred.

| ID | Threat | Severity | Trigger | Implemented control and residual risk |
|---|---|---|---|---|
| TM-01 | Frozen time prevents normal timeout progress | Medium | PM enters without stopping competing activity | The contract requires parked CPUs, quiesced readers, programming and NMI/watchdog activity. No production caller exists; HAL cannot verify system-wide quiescence. |
| TM-02 | NMI read deadlocks the writer | Medium | NMI interrupts an odd-sequence update | Writers mask ordinary IRQs/preemption; PM must separately quiesce NMI clock readers. Transitions are not NMI-safe. |
| TM-03 | Lost token leaves time frozen | Medium | Owner drops the token before resume | Private non-copyable must_use token and explicit abort/resume contract; Drop cannot substitute for restoring hardware access. |
| TM-04 | Invalid counter mapping | Medium | Frequency changes or unexpected runtime counter discontinuity | Frozen-interval changes are rebased; running continuity remains the backend's obligation. Frequency switching is unsupported. |

AlreadySuspended, StaleToken and TransitionInProgress do not change the mapping.
IdentityExhausted is checked with room reserved for resume. Logical tick sums
saturate rather than wrap. Backend sampling must not unwind while the sequence
is odd. Readers depend on writer progress, which remains a privileged PM
control-plane obligation. The tests do not change the live kernel clock and do
not prove real S3, device ordering or multiprocessor coordination. No user
private data is processed by this layer.

## Event restoration boundary

The time facade trusts initialized per-CPU event state, restored interrupt
controller/device mappings and routing, masked IRQs and PM-quiesced competing
programming/repair. It adds no raw memory access or source calibration. The
provider leaves the event stopped; final rearm requires restored timekeeping
and separate software deadline arbitration. No production PM caller is added.

## Page-table storage lifetime references

`PagingHandlerImpl::alloc_frame` obtains kalloc's single-page `PageRef` range from the global
single-page allocation API and transfers one hold to `page_table` as a PA.
`dealloc_frame` restores and consumes that exact transfer with
`PageRef::from_raw(pa, 1)`. The allocation adapter drops the length-one component
of the generic (PA, page-count) raw pair; this table-storage interface remains
PA-only and is separate from the data-reference `(PA, bytes)` provider.
The original `UsageKind` records only allocation accounting; final `PageRef::drop`
returns the page and records the release directly in kalloc. Borrowed root entries
do not own their source table pages and do not restore or release those holds.

The table owner must end hardware use and complete required translation
retirement before destroying owned storage. The existing AArch64 user-ASID
invalidation path in `PageTable64::drop` remains the final reclamation gate:
failed completion preserves table frames and all unrecovered managed data
transfers. Reference counting retains storage; it does not replace address-space
exit, remote-CPU coordination or TLB shootdown. Managed data leaves use the
separate provider below.

`ut-tests/paging_page_refs.rs` verifies table-page transfers, inactive tree
destruction and borrowed root entries that do not release source storage. Raw
restoration consumes only the handler's exact transfer. A live observer keeps
the page unavailable to another allocation; after the last hold ends, the page
is reused from the local PCP. IRQ and preemption exclusion makes this reuse
check deterministic. The tests do not forge raw releases to inspect lifetimes.

## IRQ Entry Safety

The unsafe functions in `src/irq/manager.rs` require the caller to uphold
`kcpu::excp::{IRQ, NMI}`'s captured-state and IRQ-exclusion contracts. Their
`NoPreempt` guards add CPU pinning for KIRQ dispatch but do not mask IRQs or
validate a fabricated vector. The ordinary checkpoint runs only after KIRQ
completion and context cleanup; dedicated NMI return must not schedule.

## PTE-owned data mapping-reference boundary

`PagingHandlerImpl::MappingReference` is `kalloc::PageRef`. The provider reports
its selected `phys_addr()` and `page_count() * PAGE_SIZE_4K`; the whole selected
range must remain allocated through Drop. A compound view can retain additional
backing without transferring that larger range into the leaf. The primary
`map(reference)`, `map_sized(reference, size)` and
`replace_if_same(reference)` APIs consume one exact mapping hold; object owners
retain their own references independently. The capability grants neither
content access nor device-completion authority and adds no reference counter.

### Unsafe transfer recovery and invariants

`PagingHandlerImpl::mapping_reference_into_raw` in `src/paging.rs` consumes
`PageRef::into_raw()` and converts its page count into bytes.
`unsafe PagingHandlerImpl::mapping_reference_from_raw(PA, bytes)` converts
bytes back to base-page count and invokes `unsafe PageRef::from_raw(PA, pages)`.
The byte length must equal the validated leaf's exact selected range, be a
multiple of `PAGE_SIZE_4K`, and describe one exclusively owned outstanding
transfer. Conversion and recovery do not increment or decrement lifetime counts.
Restoring the same pair twice, using a larger containing compound allocation, or
recovering an arbitrary valid PA would violate the provider contract.

The safe transaction paths in `mm/page_table/src/table64.rs` enforce admission
geometry, address coverage, subtree mutability and slot/snapshot checks before
raw transfer; no fallible step occurs between transfer and managed publication.
A private architecture category plus normalized PA and size identify the
outstanding transfer. Category is excluded from public `PagingFlags`; updates
that retain the leaf preserve it, and raw installs never create it. Category-bit
selection requires each architecture's encoding audit. A PTE copy or snapshot
is descriptive and never acquires an independent hold. No permanent per-leaf
owner index or index-node heap-OOM path remains.

On removal/replacement, the generic code captures the old leaf before clearing
or overwriting it, records required invalidation, then calls the unsafe provider
only for the captured managed category and exact old `(PA, leaf bytes)`. This
recovers the original hold without acquiring another one and moves it into
`retired_mapping_refs`. Raw leaves never enter this recovery path; a raw-only
`Infallible` provider must never restore a mapping reference.

Failed admission or a stale snapshot drops only the unpublished input and leaves
the current transfer unchanged. Empty initial permissions install no leaf;
empty matched replacement retires the old managed transfer and drops the new
unpublished input. A successful replacement returns the old snapshot without
transferring ownership. `remap_raw` and `replace_if_same_raw` reject a changed
normalized PA on a managed leaf; same-PA updates and nonempty `protect` preserve
category, while clearing permissions retires the old transfer.

### Retirement, failure and borrowing

The page-table guard retains recovered holds until confirmed TLB completion,
including full invalidation for huge leaves removed through interior addresses.
Retirement remains bounded at 16 inline holds and at most 256 with fallible
spill. Full capacity or spill OOM attempts an intermediate drain with the
recovered hold still live. Unconfirmed completion retains holds and a full
retry; an unstorable hold or unresolved guard Drop stays pinned. Repeated
failure can exhaust RAM, and no asynchronous reaper is supplied.

Automatic `finish()` / Drop needs no object-owner completion check to retain
managed mapping holds. `TlbFlushReceipt::had_pending()` describes batching,
without proving completion. Synchronous revocation still requires successful
`try_finish()` and handling of `NotReady`, `Deferred`, `TimedOut` or
`UnsupportedScope`; a live reference cannot substitute for permission
revocation or remote translation retirement.

Table destruction is separate from active-leaf retirement: the owner must stop
hardware use and future reactivation, complete required retirement and end
borrower access. The inactive-tree walk recovers each owned base/huge transfer
once before freeing its descriptor frame. Borrowed roots are skipped, and the
existing final AArch64 user-ASID completion gate preserves all frames and
unrecovered transfers on failure. `copy_from` borrows source transfers without
cloning; it checks the entire destination root range for managed leaves in
owned subtrees before any destructive overwrite. Destination managed leaves
must first be unmapped and retired. Source lifetime, shared-translation
invalidation and alias synchronization remain caller obligations.

| ID | Threat or failure | Trigger | Control and residual risk |
|----|-------------------|---------|---------------------------|
| MP-01 | Invalid or double recovery corrupts lifetime counts. | Wrong byte/page conversion, copied PTE treated as another owner, or recovery of a raw leaf. | Exact-range provider inverse and private category authorize one recovery after old-leaf capture; raw/borrowed entries never supply another transfer. Provider and architecture encoding correctness remain trusted. |
| MP-02 | Old data is reused under stale translations. | A cleared/replaced mapping loses its last hold before completion. | The recovered hold survives bounded spill/drain failure and unconfirmed completion; unresolved holds stay pinned. This prevents managed reuse but can leak RAM and does not revoke permissions. |
| MP-03 | Raw backing is released after an unconfirmed shootdown. | An existing PA-only release path uses `finish()`. | Raw names expose the unchanged external lifetime contract. Migration must transfer a mapping hold or retain backing on `try_finish()` failure; that production migration is deferred. |

PA-only callers mechanically use `map_raw`, `map_region_raw`, `remap_raw` and
`replace_if_same_raw`. Allocation/field/destructor migration for production
anon/pagecache/filemap/memspace backing is explicitly deferred. A nearby
`PageRef` does not register a guard-owned hold, and these raw paths retain the
risk described in MP-03. No alias/fork ownership migration, huge split/merge,
public cloning interface or content-access protocol is added.

Real `PageRef` inventory/reuse and exact/compound tests remain in
`ut-tests/paging_page_refs.rs`, using `map`, `map_sized` and
`replace_if_same` as the reference-consuming handles and explicit raw names for
PA fixtures. Together with generic transaction fixtures they check ownership
conservation and release ordering. Their page tables are inactive; the tests do
not establish real hardware shootdown, private-bit encoding safety or complete
production backing lifetime correctness.
