# kcpu_slot 访问模型：对齐 Asterinas

本文记录把 X-Kernel 全部 `kcpu_slot` 访问统一为 Asterinas 风格访问模型的
设计。目标：普通调用点不再出现裸 `current_base_raw()`，raw 访问只保留在
`kcpu_slot` 自身实现与 x86 TSS 汇编桥接。

## 背景

迁移完成后仍有两类不一致：

- 访问宏/裸基址：`active_exception_context.rs` 的 `local_slot!`、
  `get_at(current_base_raw())`、`ref_at/ref_mut_at(current_base_raw())`、
  `read_at/write_at(current_base_raw())`。
- 这些访问把「当前 CPU 引用」以裸 `'static` 或裸指针形式暴露，安全不变量
  只靠注释，且与仓库正在推的 `CpuPin`/`CpuLocalGuard` 契约不一致。

Asterinas 的做法（`ostd/src/cpu/local/`）是三层：

1. `CpuLocal::get_with(&irq_guard) -> CpuLocalDerefGuard`：当前 CPU 引用被
   IRQ guard 的生命周期约束；
2. `CpuLocal::get_on_cpu(cpu) -> &T`（要求 `T: Sync`）：远端/按 CPU 访问；
3. `CpuLocalCell` 的单指令安全方法 `load/store/add_assign/...`：`&'static self`，
   由架构单指令（`SingleInstruction*` trait）实现。

声明侧仍用宏（`cpu_local!`/`cpu_local_cell!`），宏只负责把 static 放进特殊段。

## X-Kernel 的三层模型

1. **自举原语**（`kcpu_slot` 内部 + 汇编）：`current_base_raw()`、模板符号。
   对应 Asterinas 的 `arch::cpu::get_base()`，用于打破
   `guard -> current_cpu() -> KCPU_ID -> guard` 递归与 x86 TSS 汇编。
2. **无 guard 的共享/按值访问**（Sync 或 Copy 载荷）：
   - `CpuArea::current()` / `kcpu_slot::current_area()`：由基址反推 CPU 槽位，
     不需要 CPU id 或 guard，供 `at`/`shared_at` 使用；
   - `CpuSlot::<T: Sync>::current_shared() -> &T`；
   - `CpuSlotCell::<T: Sync>::shared_current() -> &T`；
   - `CpuSlotCell::<T: Copy + SingleInstruction*>::load_current()/store_current()`。
3. **guard 约束的当前 CPU 访问**（非 Sync 载荷）：`CpuSlot::get(&guard)`、
   `CpuSlotCell::get(&guard)`、`read/write(&guard)`；可变借用只保留
   `unsafe get_mut_unchecked(&pin)`。
4. **已发布 area 的远端访问**：`CpuSlot::at(&area)`、`CpuSlotCell::shared_at(area)`。

## 单指令访问与兜底的层次问题

Asterinas 的 `SingleInstruction*` 兜底实现调用 `crate::irq::disable_local()`，
因为 `ostd` 是单体 crate。X-Kernel 中 `kcpu_slot` 位于 `karch` 之下，不能依赖
`karch`，而正确的关中断逻辑（尤其 AArch64 的 PMR / ALLINT / pseudo-NMI
不变量）由 `karch` 拥有。

因此采用 **B2**：`kcpu_slot` 定义 `kiface` 接口，`karch` 提供实现：

```rust
#[kiface::interface]
pub trait CpuLocalIrqIf {
    fn save_and_disable() -> usize;
    fn restore(state: usize);
}

#[kiface::provide]
impl kiface_cpu_api::CpuLocalIrqIf {
    fn save_and_disable() -> usize { crate::irq::save_irq_and_disable() }
    fn restore(state: usize) { crate::irq::restore_irq(state) }
}
```

这与现有 `kspin::CurrentCpuIf`（下层定义、`kplat` 提供）同构，不引入依赖环。

- x86_64：真正单指令 `gs:[offset]`，不经过接口；
- aarch64/riscv/loongarch：经 `CpuLocalIrqIf::save_and_disable` 关中断后做
  普通读改写，再 `restore`（等价 Asterinas 的兜底语义，但复用 `karch` 的正确
  mask 逻辑）；
- 不使用 `min_specialization`：每个 arch 模块对固定标量集
  （`u8..u64`/`usize`/`i8..i64`/`isize`/`bool`，以及指针类型）分别实现
  `SingleInstruction*`。

## kcpu_slot API 变更

新增：

- `kcpu_slot::current_area() -> Option<CpuArea>`：由基址反推槽位下标
  （`(base - _cpu_slot_area_start) / stride`，越界或未发布返回 `None`）。
- `CpuSlot::current_shared() -> &T`（`T: Sync`）。
- `CpuSlotCell::shared_current() -> &T`（`T: Sync`）。
- `CpuSlotCell::load_current()/store_current()`（`T: Copy + SingleInstruction*`）。
- `CpuSlotCell::get(&guard)`（guard 约束的共享引用）。不给安全的可变借用：`get_mut`
  返回 `&mut T` 时，两个不同 guard 可同时命名同一 slot，安全代码即可制造
  `&T`+`&mut T` 别名（UB），因此只保留 `unsafe get_mut_unchecked(&pin)`。

删除（public raw 入口）：

- `CpuSlot::{get_at, get_unchecked, get_mut_unchecked}`、
  `CpuSlotCell::{get_at, read_at, write_at, ref_at, ref_mut_at, read_unchecked,
  write_unchecked, get_unchecked}`。逐指针 helper 全部降为 `pub(crate)`，不对外暴露。
- `kcpu_slot::current_base_raw()` 已删除（其功能由 `current_shared`/`shared_current`/
  `current_area`/`current_shared_mut` 覆盖）。

唯一保留的 public raw 入口是
`CpuSlotCell::current_shared_mut()`，仅供 x86 GDT bring-up（`load()` 要求
`'static`）；这是与 Asterinas `as_mut_ptr`/`SingleInstruction*` 等价的自举逃生口。

## 站点迁移

| 站点 | 现状 | 目标 |
|---|---|---|
| `platforms/kplat/src/cpu.rs` `KCPU_ID`/`KCPU_BSP` | `read_at/write_at(base)` | `load_current/store_current` |
| `arch/khal/src/percpu.rs` `CURRENT_TASK_PTR` | `read_at/write_at(base)` | `load_current/store_current` |
| `arch/kcpu/src/active_exception_context.rs` | `local_slot!` + 裸 `get_at` | `current_shared()`，删宏改函数 |
| `arch/karch/src/aarch64/irq.rs` `ALLINT_ACTIVE`/`READY` | 裸 `get_at` | `current_shared()` |
| `arch/kirq/src/bottom_half/context.rs` | `get_unchecked(&pin).get()` | `get(&guard)` + `lock()` |
| `task/ktask/src/run_queue/mod.rs` | `ref_at/ref_mut_at(base)` | `get(&guard)` / `unsafe get_mut_unchecked(&pin)` / `at(&area)` |
| `io/watchdog/src/{watchdog_task,init}.rs` | `ref_at/ref_mut_at(base)` | `get(&guard)` / `unsafe get_mut_unchecked(&pin)` |
| `platforms/kplat-aarch64/src/peripherals/pmu.rs` | `ref_mut_at(base)` | `unsafe get_mut_unchecked(&pin)` |
| `mm/kalloc` `AllocLocalOwner` | `get_unchecked(owner.pin())` | owner 升级为 `NoPreemptIrqSave`，`unsafe get_mut_unchecked(owner.guard())` |
| `arch/kcpu/src/x86_64/gdt.rs` `GDT` | `ref_mut_at(base)` | `current_shared_mut()`（`GlobalDescriptorTable::load` 要求 `'static`，唯一保留的 raw 边界） |
| `arch/kcpu/src/x86_64/gdt.rs` `TSS` | 裸 `get_at` + 汇编 | `current_shared()`；汇编仍经 `.cpu_slot.template` 偏移 |
| `arch/kipi` | area/guard | 已符合，不改 |

## 安全不变量

- Sync 载荷共享：存储随内核永驻 + `T: Sync`，返回 `&T` sound（同 Asterinas
  `get_on_cpu`）。
- Copy 标量单指令：单条读/写不可分割；兜底架构在 `CpuLocalIrqIf` 关中断下
  完成，仍是本级 CPU 访问。
- 非 Sync：必须 `CpuLocalGuard`；`&mut guard` 在编译期禁止重入借用。
- `current_area()` 不依赖 guard / CPU id，消除自举递归。
- 汇编：仅 x86 TSS 一处。

## 验证

分模块提交，每步 `make build`；覆盖 aarch64（qemu/crosvm/virtcca）、x86_64、
riscv64；最后 `make run`、`make unittest`、`fmt`/`clippy`。
