IRQ 27 当前流程
当前 kvmm-dev 里 27 号中断的路径可以分成 5 段：
1. guest 配置虚拟 timer。
2. guest 运行时 virtual timer 到期导致一次 VM exit，或者 guest 因其他原因 exit。
3. host 在 exit handler 里检查 guest virtual timer 是否到期。
4. 如果到期，把 IRQ 27 标记到 vGIC pending bitmap。
5. 下一次进入 guest 前，VgicHook::on_entry() 把 pending IRQ 27 写进 GICH LR，guest 通过 GICV/GICC 看到中断。
关键点是：现在 IRQ 27 的注入依赖 VM exit 后的软件检查，不是独立异步注入。
所以你说的“只有 vCPU 被打断才可能注入中断”，在当前代码里确实成立。
启动时 GIC Wiring
启动 FreeRTOS 的时候，device.rs 里 handle_boot_cmd() 做了这些事情：
virt/kvmm/src/device.rs:129-136
let (uart, console_rx) = vpl011::Vpl011::new(vm_id);
vm.shared().mmio_bus().lock().register(Box::new(uart));

#[cfg(target_arch = "aarch64")]
setup_gic(&mut vm)?;
setup_gic() 负责建立最小 GIC 模型：
virt/kvmm/src/device.rs:186-223
const GICC_BASE: u64 = 0x0801_0000;
const GICH_BASE: u64 = 0x0803_0000;
const GICV_BASE: u64 = 0x0804_0000;
它做三件事：
1. guest 访问 GICC_BASE 时，stage2 实际映射到硬件 GICV_BASE。
gm.map_region(GICC_BASE, GICV_BASE, GIC_IF_SIZE, GuestPerm::DeviceRW)
2. host 自己 iomap GICH_BASE，用于写 GICH list registers。
let gich_va = memspace::iomap_device(... GICH_BASE ...)
3. 创建 Vgic 和 Vgicd。
let vgic = vgic::Vgic::new(1, gich_va.as_usize());
let vgicd = vgicd::Vgicd::new(vgic.clone());
vm.shared().mmio_bus().lock().register(Box::new(vgicd));
vm.shared().set_vgic(vgic);
然后在创建 vCPU 后，把 VgicHook 挂到 vCPU：
virt/kvmm/src/device.rs:156-159
if let Some(vgic) = vm.shared().vgic() {
    vcpu.hooks
        .push(alloc::boxed::Box::new(vgic::VgicHook::new(vgic)));
}
所以当前设计是：
- GICD：软件模拟，MMIO trap 到 vgicd.rs。
- GICC：guest 以为自己访问 GICC，stage2 实际映射到硬件 GICV。
- GICH：host-only，用 VgicHook 在 guest entry/exit 前后保存恢复。
- vIRQ 注入：host 写 GICH LR。
Guest Exit 时保存 Timer
guest 进入前，汇编会恢复 guest virtual timer：
virt/kvmm/src/arch/aarch64/el2_vmcs.S:99-105
ldr  x2, [x0, #560]         // vcpu->cntv_cval
msr  S3_5_C14_C3_2, x2      // CNTV_CVAL_EL02
ldr  x2, [x0, #552]         // vcpu->cntv_ctl
msr  S3_5_C14_C3_1, x2      // CNTV_CTL_EL02
isb
guest exit 时，汇编保存 guest virtual timer 状态，但不禁用 timer：
virt/kvmm/src/arch/aarch64/el2_vmcs.S:183-192
mrs  x1, S3_5_C14_C3_1      // CNTV_CTL_EL02
mrs  x2, S3_5_C14_C3_2      // CNTV_CVAL_EL02
str  x1, [x0, #552]         // vcpu->cntv_ctl
str  x2, [x0, #560]         // vcpu->cntv_cval
// msr  S3_5_C14_C3_1, xzr     // disable timer
isb
也就是说 guest virtual timer 的 CNTV_CTL/CVAL 是 per-vCPU 保存/恢复的，但物理
PPI 27 仍允许在 host IRQ path 中被观察到。这样 guest-running 时 timer 到期能经
host VBAR/GIC 路径打断当前 vCPU。
IRQ 27 产生点
27 号中断不是在硬件 timer IRQ handler 里直接注入的。hard IRQ 只唤醒当前
pCPU 发布的 owner task；真正注入由 vCPU thread 根据该 vCPU 保存的 CNTV state
调用 check_vtimer() 完成。当前在 guest entry 前会无条件按当前 vCPU 的 deadline
重算：
virt/kvmm/src/vdev/vtimer.rs
fn on_entry(&mut self, vcpu: &mut Vcpu<Aarch64Vhe>) {
    publish_owner_for_current_cpu();
    check_vtimer(vcpu);
}
check_vtimer() 读取保存下来的 guest CNTV_CTL 和 CNTV_CVAL：
virt/kvmm/src/arch/aarch64/mod.rs:148-164
fn check_vtimer(vcpu: &mut Vcpu<Aarch64Vhe>) {
    const CTL_ENABLE: u64 = 1 << 0;
    const CTL_IMASK: u64 = 1 << 1;
    let ctl = vcpu.arch.cntv_ctl;
    if ctl & CTL_ENABLE == 0 || ctl & CTL_IMASK != 0 {
        return;
    }

    let now: u64;
    let cntvoff: u64;
    unsafe {
        core::arch::asm!("mrs {}, cntpct_el0", out(reg) now);
        core::arch::asm!("mrs {}, cntvoff_el2", out(reg) cntvoff);
    }

    if now >= vcpu.arch.cntv_cval.wrapping_add(cntvoff) {
        vcpu.vm.inject_irq(vcpu.vcpu_id, 27);
    }
}
判断条件是：
CNTV_CTL.ENABLE == 1
CNTV_CTL.IMASK == 0
CNTPCT_EL0 >= CNTV_CVAL_EL02 + CNTVOFF_EL2
满足后调用：
vcpu.vm.inject_irq(vcpu.vcpu_id, 27);
这个位置非常关键：只有 guest 已经 exit 到 host，Rust 的 exit_handler() 被执行了，才会检查 27 是否该 pending。
如果 guest 一直不 exit，host 只能靠物理 IRQ 让 guest exit。当前 el2_enter_guest 设置了：
virt/kvmm/src/arch/aarch64/el2_vmcs.S:74-83
bic  x2, x2, #(1 << 27)    // clear TGE
orr  x2, x2, #(1 << 13)    // set TWI
orr  x2, x2, #(1 << 4)     // set IMO
orr  x2, x2, #(1 << 3)     // set FMO
msr  hcr_el2, x2
IMO/FMO 会把物理 IRQ/FIQ route 到 EL2。也就是说 guest 跑 tight loop 时，host timer IRQ 可以让它 exit。但“27 号虚拟中断是否到期”的检查仍然是在这个 exit 之后做的。
VmShared::inject_irq
check_vtimer() 调用的是 VmShared::inject_irq()：
virt/kvmm/src/vm.rs:317-337
pub fn inject_irq(&self, vcpu_id: u32, irq: u32) {
    if vcpu_id as usize >= self.nr_vcpus {
        return;
    }
    if let Some(vgic) = self.vgic.lock().clone() {
        vgic.set_pending(vcpu_id, irq);
    }

    if self.vcpu_run_state(vcpu_id) == VcpuRunState::WfiSleeping
        && let Some(task) = self.vcpu_tasks[vcpu_id as usize].lock().clone()
    {
        ktask::interrupt_task(&task, true);
    }
}
这里做两件事：
1. 如果有 vGIC，调用 vgic.set_pending(vcpu_id, irq)。
2. 如果 vCPU 当前在 WfiSleeping，唤醒 vCPU 线程。
它明确有一个 TODO：
virt/kvmm/src/vm.rs:329-331
// TODO(irq): if the target vCPU is RunningGuest on another pCPU, send a
//            physical IPI to force an exit + re-injection
这正是当前设计缺口：running guest 状态下没有主动 kick。
但是对 IRQ 27 来说，还有更深一层问题：当前 27 本身也是在 exit 后才被检查出来的，所以如果没有 host IRQ/其他 trap，inject_irq(27) 根本不会被调用。
vGIC Pending Bitmap
Vgic::set_pending() 很简单：
virt/kvmm/src/vdev/vgic.rs:94-101
pub fn set_pending(&self, vcpu: u32, irq: u32) {
    if (vcpu as usize) < self.nr_vcpus && irq < MAX_IRQS {
        self.cores[vcpu as usize]
            .pending
            .fetch_or(1u64 << irq, Ordering::Release);
    }
}
当前 vGIC 只支持 MAX_IRQS = 64，所以 IRQ 27 在范围内。
pending 是每 vCPU 一个 AtomicU64：
virt/kvmm/src/vdev/vgic.rs:55-59
struct Core {
    pending: AtomicU64,
    hw: UnsafeCell<Hw>,
}
这里 pending 只是一个 bitmap，不区分 edge/level，不保存 active/pending 状态机，不看 GICD enable，不看 priority mask，也不处理 redistributor，因为当前是 GICv2 最小模型。
Entry 前写 GICH LR
真正把 IRQ 27 交给硬件 GIC virtual interface 的地方是 VgicHook::on_entry()：
virt/kvmm/src/vdev/vgic.rs:134-176
流程：
1. 取出当前 vCPU 的 Core。
2. 取出 cached hw.lr。
3. pending.swap(0) 把 pending bitmap 清空并拿到本次要注入的 IRQ 集合。
4. 遍历每个 pending bit。
5. 如果这个 IRQ 已经在 LR 里，跳过。
6. 找一个空 LR，把 IRQ 写进去。
7. 写 GICH_VMCR、GICH_APR、所有 LR。
8. 写 GICH_HCR = HCR_EN 开启 virtual interface。
关键代码：
let mut pending = core.pending.swap(0, Ordering::Acquire);
while pending != 0 {
    let irq = pending.trailing_zeros();
    pending &= pending - 1;

    if lr
        .iter()
        .any(|&l| l & LR_VINTID_MASK == irq && l & LR_STATE_MASK != 0)
    {
        continue;
    }

    match lr.iter().position(|&l| l & LR_STATE_MASK == 0) {
        Some(slot) => lr[slot] = LR_PRIORITY | LR_STATE_PENDING | irq,
        None => {
            core.pending.fetch_or(1u64 << irq, Ordering::Release);
            break;
        }
    }
}
对 IRQ 27，写进 LR 的值大概是：
State   = Pending
Priority = 0x14
VINTID  = 27
即：
LR_PRIORITY | LR_STATE_PENDING | 27
然后：
vgic.gich_write(GICH_LR0 + i * 4, l);
vgic.gich_write(GICH_HCR, HCR_EN);
之后 guest 进入 EL1，访问 GICC 实际走 GICV，硬件 virtual CPU interface 会把 LR 里的 vIRQ 27 呈现给 guest。
Exit 后读回 GICH LR
guest exit 后，vmm_run_vcpu() 在 IRQ masked window 内调用 hook 的 on_exit()：
virt/kvmm/src/vcpu.rs:162-166
A::save_guest_ctx(vcpu);
for hook in &mut vcpu.hooks {
    hook.on_exit(vid);
}
VgicHook::on_exit() 会保存 VMCR/APR/LR，并根据 ELSR 清掉空 LR：
virt/kvmm/src/vdev/vgic.rs:178-203
hw.vmcr = vgic.gich_read(GICH_VMCR);
hw.apr = vgic.gich_read(GICH_APR);
let elsr0 = vgic.gich_read(GICH_ELSR0);
for (i, l) in hw.lr.iter_mut().enumerate() {
    *l = vgic.gich_read(GICH_LR0 + i * 4);
    if elsr0 & (1 << i) != 0 {
        *l = 0;
    }
}
vgic.gich_write(GICH_HCR, 0);
这意味着：
- 如果 guest ack/EOI 了 IRQ 27，LR 可能变 empty，ELSR0 对应 bit 为 1，然后 cached LR 被清 0。
- 如果 guest 没处理完，LR 状态会被保存，下次 entry 再恢复。
- 当前没有显式维护 active/pending 状态机，只依赖硬件 LR 状态和 ELSR0。
Run Loop 时序
完整 vCPU run loop 里，GIC hook 的位置是：
virt/kvmm/src/vcpu.rs:116-170
mask host IRQ

当前补充路径：host 侧 IRQ27 handler 不保存 per-CPU fired latch，也不直接注入
vIRQ27。guest exit 后，exit_handler() 短暂打开 host IRQ，让 host VBAR/GIC 路径
ACK/EOI pending 的 physical INTID27，然后继续按 exit type 处理。

vCPU 下次 entry 前，HostVtimerHook 会无条件按当前 vCPU 保存的 CNTV_CTL/CVAL 调
check_vtimer()。如果 IRQ27 是在 host 正在运行普通线程或其它 VM 时到达的，vCPU
重新被调度回来后会在写 GICH LR 之前按自己的 deadline 决定是否注入，避免
per-pCPU host IRQ 被同 pCPU 上的其它 VM 错归属。

host_vtimer_irq_handler() 通过 IRQ-safe 的 per-CPU owner task 表唤醒最后在该 CPU
上运行 guest vtimer 状态的 vCPU task。owner 在 HostVtimerHook::on_entry() 中发布，
在 vCPU teardown 时清除；IRQ handler 只克隆 KtaskRef 并 interrupt_task(..., true)，
不拿 VM mutex、不访问 vGIC。这样 IRQ27 在 host EL0 或其它 host context 到达时，
可以请求调度 vCPU；真正的 check_vtimer() 和 vIRQ27 注入仍在 vCPU thread 的 entry
安全上下文完成。
set pcpu
activate stage2
restore guest ctx
VgicHook::on_entry()
set state = RunningGuest
enter_guest()

guest exits

set pcpu = -1
set state = HostHandlingExit
save_guest_ctx
VgicHook::on_exit()
unmask host IRQ
exit_handler()
所以 IRQ 27 的一个典型 tick 周期是：
guest 写 CNTV_CVAL / CNTV_CTL
guest eret 运行
guest virtual timer 到期或 host timer IRQ 抢占导致 exit
exit stub 保存 CNTV_CTL/CVAL，不 disable CNTV_CTL
Rust exit_handler() 打开 host IRQ，让 host VBAR/GIC ACK/EOI INTID27 并消费 latch
下一次 before_guest_entry() 调 check_vtimer()
check_vtimer() 发现 now >= CVAL + CNTVOFF
VmShared::inject_irq(vcpu, 27)
Vgic::set_pending(27)
run loop continue
下一轮 entry 前 VgicHook::on_entry()
pending bitmap -> GICH LR pending IRQ 27
eret 回 guest
guest 从 GICV/GICC 看到 vIRQ 27
guest ISR ack/EOI
下次 exit 时 VgicHook::on_exit() 保存/清理 LR
为什么 FreeRTOS 能跑
FreeRTOS benchmark 能跑，说明当前路径在单 vCPU、周期性有 exit 的场景下足够工作。
原因大概率是：
- HCR_EL2.IMO=1 让 host 物理 IRQ 能把 guest 打出来。
- run loop 也有周期性 yield，guest 不是永远占住。
- guest 本身可能有 WFI、MMIO、GIC 访问等 trap。
- 每次 exit 后 check_vtimer() 都会补一次 vPPI 27。
- vPPI 27 被 pending 后，下一次 guest entry 前写入 GICH LR。
但这个模型本质上是“exit-driven timer injection”，不是“timer event-driven injection”。
当前设计问题
当前流程里最核心的问题有这些：
1. IRQ 27 的产生依赖 exit_handler() 执行。
check_vtimer() 只在 VM exit 后跑。如果 guest 长时间不 exit，就不能及时发现 guest virtual timer 到期。
2. inject_irq() 对 RunningGuest 没有 kick。
代码里已经写了 TODO。当前如果外部设备调用 inject_irq()，而目标 vCPU 正在别的 pCPU 上跑 guest，不会发 IPI 让它退出并重载 LR。这个 pending 只能等下一次自然 exit 才会进入 GICH LR。
3. vIRQ 27 不是由 host timer deadline 驱动。
当前没有把 guest CNTV_CVAL 转换成 host hrtimer/clockevent，也没有在 guest timer 到期时由 host timer callback 设置 pending + kick vCPU。它只是 exit 后检查。
4. guest virtual timer 被 exit stub 禁用了。
这避免 host 阶段漏出 IRQ 27，但也说明 host 阶段不会靠 guest CNTV 自己继续产生 interrupt。host 必须自己维护“这个 guest timer deadline 到了”的事件。
5. vGIC 是 LR-drain 模型，不是完整 GIC state machine。
当前 pending bitmap 到 LR 的路径能跑 FreeRTOS，但没有完整处理 enable/disable、level/edge、active/pending、priority、targeting、多 vCPU、maintenance interrupt 等语义。
6. GICD enable 状态目前不参与 injection。
vgicd.rs 记录了 enabled，但 Vgic::set_pending() 不检查它。对 bring-up 可以接受，但之后要明确边界：GICD 负责 interrupt config/state，VGIC core 负责 pending/active/route/LR sync，而不是所有源都直接绕过 distributor enable。
对 27 号中断的准确结论
当前 27 号中断不是这样：
guest CVAL 到期 -> host 定时器事件 -> vGIC pending -> kick vCPU -> LR -> guest
而是这样：
guest 因某种原因 exit -> host 检查 guest CVAL 是否已过期 -> vGIC pending -> 下一次 entry 写 LR -> guest
所以它有天然延迟：
timer 到期时间
到下一次 VM exit 的时间
到下一次 VgicHook::on_entry 写 LR 的时间
guest 实际接收 IRQ 的时间
这也解释了你日志里的 IRQ latency：
[IRQ Latency] avg=6185552 ns
6ms 级别的 latency 很像“不是精确 deadline 驱动，而是靠 exit/调度机会补发”的结果。
我建议后续重构时先明确边界
先不改代码的话，我建议你这版设计先把 27 号路径拆成三个明确职责：
1. vtimer 层：负责 guest virtual timer deadline 的保存、重编程 host timer、到期回调。
2. irq routing 层：负责把一个 interrupt source 变成某个 vCPU 的 vIRQ pending，并决定是否 kick。
3. vgic 层：负责 GIC state 和 GICH LR sync，不负责猜测 timer 是否到期。
这样 IRQ 27 的目标流程应该变成：
guest 写 CNTV_CVAL/CNTV_CTL 或 exit 保存 timer state
vtimer 根据 guest deadline arm host timer
host timer callback 到期
set vIRQ 27 pending
如果 vCPU RunningGuest：IPI/kick 让它 exit
如果 WfiSleeping：interrupt_task 唤醒
下一次 entry 前 vgic 把 27 写入 LR
guest 通过 GICV 收到 PPI 27
当前代码里已经有一半基础：
- VmShared::inject_irq() 是统一入口。
- Vgic::set_pending() 是 pending bitmap。
- VgicHook 已经在正确的 IRQ masked world-switch window 里同步 GICH。
- VcpuRunState 已经能区分 RunningGuest 和 WfiSleeping。
- 缺的是“timer-driven source”和“running guest kick”。
