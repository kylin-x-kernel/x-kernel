> Historical note: this document describes an older design that used a
> per-CPU `HOST_VTIMER_IRQ_FIRED` latch. Current code removed that latch; the
> host IRQ route wakes the owner vCPU task, and vtimer injection is recomputed
> from the current vCPU's saved `CNTV_CTL`/`CNTV_CVAL` before guest entry.

你的判断是对的：现在这版只能降低“vCPU 正在运行 guest 时”的 IRQ27 延迟，不能解决“host 正在跑 EL0/普通线程时 IRQ27 到期，但 vCPU 没被及时调度回来”的延迟。
现在路径解决的是这个场景：
vCPU thread 正在 guest 中运行
  |
guest CNTV 到期，physical INTID27 pending
  |
HCR_EL2.IMO=1，把 physical IRQ route 到 EL2
  |
进入 kvmm_guest_vec_table，发生 VM exit
  |
exit_handler 打开 host IRQ
  |
host VBAR ACK/EOI INTID27
  |
host_vtimer_irq_handler 设置 FIRED
  |
exit_handler check_vtimer()
  |
注入 guest vIRQ27
  |
下一次 enter guest，guest 收到 tick
这个场景下确实比旧版好，因为不再依赖 guest 自然 VM exit。
但你说的这个场景仍然没有完整解决：
当前 pCPU 正在跑 x-kernel host EL0 用户态线程
  |
physical INTID27 到期
  |
host VBAR 处理 IRQ27
  |
host_vtimer_irq_handler 设置 HOST_VTIMER_IRQ_FIRED = true
  |
IRQ return 回 host EL0
  |
vCPU thread 没有被唤醒/抢占调度
  |
guest vIRQ27 没有立即注入
原因是当前 host_vtimer_irq_handler() 只有这个动作：
HOST_VTIMER_IRQ_FIRED.write_current_raw(true);
它没有：
vcpu.vm.inject_irq(...)
ktask::interrupt_task(vcpu_task)
设置 vCPU need_kick / runnable priority
请求 host scheduler 抢占当前 EL0 task
所以它只是一个 per-CPU latch，不是调度事件。
更关键的是：这个 latch 是 per-CPU 的，不知道具体哪个 VM/vCPU 拥有这个 timer。当前真正消费它的地方只有：
exit_handler()
也就是 vCPU thread 已经从 guest exit 回来了之后：
let host_vtimer_irq_fired = leave_host_vtimer_irq_window();
if host_vtimer_irq_fired {
    check_vtimer(vcpu);
}
如果 IRQ27 是在普通 host EL0 线程运行时发生的，就没有当前 vcpu 可用，handler 也没法直接调用 check_vtimer(vcpu)。
所以当前行为大概是：
IRQ27 到期时 vCPU 正在运行 guest:
  能立刻造成 VM exit，延迟下降明显。

IRQ27 到期时 host 正在运行别的线程:
  只能记录 FIRED，vCPU 不会马上被调度回来。
  等 vCPU thread 之后自然被调度、再次进入/退出 guest，才可能注入。
甚至还有一个细节：如果 vCPU thread 是在 yield_now() 后被调度出去的，等它再次被调度回来时，它可能直接继续 loop 然后 enter_guest()，不会先跑 exit_handler()。这时之前的 HOST_VTIMER_IRQ_FIRED 还在 per-CPU 上，但没有立即消费。guest timer 如果仍然 expired，重新 entry 后可能马上再次触发 EL2 IRQ exit，再走 exit_handler() 注入。这会多一次无意义 guest entry/exit，延迟仍然偏大。
所以你的结论可以更精确地说：
当前 IRQ27 host VBAR 路径解决了“guest-running 时 timer 到期无法打断 guest”的问题；
但没有解决“vCPU not-running 时 timer 到期无法立即唤醒/调度 vCPU”的问题。
要真正降低这类延迟，需要增加一个 vCPU wake/kick 机制
理想结构应该是：
host_vtimer_irq_handler()
  |
  | 找到当前 pCPU 上绑定的 running vCPU 或 owner vCPU
  |
  | 标记 timer fired / pending tick
  |
  | 如果 vCPU 正在 guest:
  |     让它通过 VM exit path 注入
  |
  | 如果 vCPU 不在 guest / 被 host deschedule:
  |     wake 或 interrupt vCPU task
  |     请求 host scheduler 尽快切到 vCPU thread
但要做到这个，需要比当前 per-CPU bool 更明确的状态。
当前只有：
HOST_VTIMER_IRQ_FIRED: per-cpu bool
它不包含：
哪个 VM
哪个 vCPU
vCPU task handle
vCPU 当前是否 RunningGuest / WfiSleeping / HostHandlingExit / Offline
VmShared 里其实已经有一些基础：
vcpu_pcpu
vcpu_run_state
vcpu_tasks
inject_irq() 里也有：
if self.vcpu_run_state(vcpu_id) == VcpuRunState::WfiSleeping {
    ktask::interrupt_task(&task, true);
}
但当前只有 WfiSleeping 会 wake。RunningGuest 跨 pCPU kick 还留着 TODO：
// TODO(irq): if the target vCPU is RunningGuest on another pCPU, send a
// physical IPI to force an exit + re-injection
你现在指出的是第三种：
target vCPU 不是 RunningGuest，也不是 WfiSleeping，而是 host scheduler 没在跑它
这时应该也要有某种：
ktask::interrupt_task(&task, true)
或者 scheduler-level wake/resched。
但直接在 hard IRQ handler 里做这件事要小心
host_vtimer_irq_handler() 是 hard IRQ context。不能随便拿复杂锁，也不能做可能阻塞的 VM 操作。所以更合理的是：
1. IRQ handler 只记录事件，但记录得更完整：
timer_owner_vcpu = ...
fired = true
2. 如果能安全拿到 KtaskRef，只做轻量 wake/kick：
ktask::interrupt_task(&task, true)
3. 真正 inject_irq() 仍放到 vCPU thread 的安全上下文里。
当前最小改进方向
可以考虑在 vCPU entry 前也消费一次 timer fired，而不只在 exit_handler 消费。
比如 run loop 在：
A::restore_guest_ctx(vcpu);
hook.on_entry(...)
enter_guest()
之前，加一个 arch hook：
A::prepare_entry(vcpu)
AArch64 里做：
if take_host_vtimer_irq_fired() {
    check_vtimer(vcpu);
}
这样至少能修掉这个问题：
host EL0 收到 IRQ27
FIRED=true
vCPU 之后被调度回来
进入 guest 前先注入 vIRQ27
这不能让 vCPU “立刻抢占 host EL0”，但能避免 vCPU 被调度回来后还要先进入 guest、再马上 exit 一次。
真正的低延迟还要加：
IRQ27 handler wake vCPU task / request resched
当前版本能不能降低 guest IRQ latency？
答案分场景：
场景
vCPU 正在 guest 中跑 tight loop，timer 到期
guest 刚好在 WFI，timer 到期
vCPU thread 正在 host exit_handler 里
pCPU 正在跑 host EL0 用户线程，vCPU 没在跑
vCPU 被调度回来但尚未 enter guest
所以你观察到的 kvmm IRQ Latency avg 仍然大概 2x QEMU bare，是合理的。我们修掉的是“必须等自然 VM exit”的大问题，但还没做到“host timer 到期立即调度 vCPU 并注入”的完整实时路径。
我建议下一步
如果要继续压 IRQ latency，下一步应该做两个补丁：
1. entry 前消费 timer fired
- 把 HOST_VTIMER_IRQ_FIRED 从只在 exit_handler() 消费，扩展到 guest entry 前也消费。
- 目标：vCPU 重新获得 CPU 后，不再多一次无效 guest entry/exit。
2. IRQ27 到期时 wake/kick owner vCPU task
- 需要建立 physical PPI27 当前 owner：
per-CPU current_vtimer_owner: Option<(VmRef, vcpu_id)>
- guest entry 时 publish owner。
- guest exit/offline 时 clear 或更新 owner。
- host IRQ handler 看到 INTID27 时，如果 owner vCPU 不是 RunningGuest，就唤醒/请求调度它。
- 真正注入仍在 vCPU thread 上完成。
这两个做完后，才算把你说的 EL0 host thread 场景也覆盖到。
