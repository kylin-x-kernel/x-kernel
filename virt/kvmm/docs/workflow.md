VM 创建

selftest_guest_mem_impl (以 guest-mem selftest 为例)
│
├─ A::percpu_hw_init()              // 每 CPU 一次硬件初始化
│   ├─ x86: vmx_check_support + vmx_percpu_init (CR fixup, VMXON)
│   ├─ aarch64: no-op (VHE 透明)
│   └─ riscv64: hext_init (设 hstatus.VTW)
│
├─ Vm::new(cfg)                     // 创建 VM 容器
│   ├─ cfg.mem_size > 0 → A::GuestMem::new(base, size)
│   │   ├─ x86: Ept::new()   — 分配 PML4/PDPT/PD，填 2MiB 大页
│   │   ├─ arm: Stage2::new() — 分配 L1/L2，填 2MiB block entries
│   │   └─ rv:  GStage::new() — 分配 root(16K)/L1×4，填 2MiB superpages
│   │
│   └─ Arc::new(VmShared { cfg, guest_mem, mmio_bus, vcpu_pcpu })
│
├─ vm.create_vcpu(0)                // 创建 Vcpu<A>
│   └─ Vcpu { arch: Default, vcpu_id, launched: false, vm: Arc::clone, hw_pages: [] }
│
├─ load_guest::<A>()                // 加载 guest 代码+栈
│   ├─ GUEST_SHARES_HOST_MMU=true (x86): 直接用 .text VA，不拷贝
│   └─ =false (arm/rv): alloc page，copy guest code，alloc stack page
│   └─ 返回 (entry_va, sp, GuestPages)
│
├─ A::init_vcpu(&mut vcpu, entry, sp)  // 初始化架构状态
│   ├─ x86: vmcs_init_vcpu
│   │   ├─ alloc VMCS page → push 到 vcpu.hw_pages
│   │   ├─ vmclear + vmptrld
│   │   ├─ 写 VMCS 所有字段 (controls, host state, guest state)
│   │   └─ GuestRip=entry, GuestRsp=sp
│   ├─ arm: 设 ELR_EL1, SP_EL1 等
│   └─ rv: 设 vsepc, sp 等
│
├─ pages.bind_to_vcpu(&mut vcpu)    // guest pages 移入 vcpu.hw_pages
│
└─ spawn_vcpu_thread(vcpu)          // 移入新线程执行
    └─ ktask::spawn_with_name(move || vmm_run_vcpu(&mut vcpu))

vCPU Run Loop

vmm_run_vcpu<A>(vcpu)
│
├─ 一次性激活 guest memory
│   ├─ x86: vmcs_enable_ept (写 EPTP 到 VMCS)
│   ├─ arm: msr vttbr_el2
│   └─ rv:  csrw hgatp
│
├─ A::restore_guest_ctx(vcpu)       // x86: no-op (VMCS 自动); arm/rv: 恢复 sys
│
└─ loop {
    ├─ set_vcpu_pcpu(id, this_cpu)  // 记录当前物理 CPU
    │
    ├─ gm.activate()                // 每次入 guest 前刷新 Stage-2 寄存器
    │   ├─ arm: msr vttbr_el2 + isb
    │   ├─ rv:  csrw hgatp + hfence.gvma
    │   └─ x86: no-op (EPT 在 VMCS 中，vmptrld 已加载)
    │
    ├─ A::enter_guest(vcpu)         // 进入 guest
    │   ├─ x86:
    │   │   ├─ vmptrld(vmcs_pa)
    │   │   ├─ vmcs_write(HostBaseGs, rdmsr(GS_BASE))  ← 修复: 刷新 GS
    │   │   └─ vmx_enter_guest (asm: 加载 GPR → vmlaunch/vmresume)
    │   ├─ arm: eret 到 EL1
    │   └─ rv:  sret 到 VS-mode
    │
    ├─ ── VM EXIT ──                // 硬件自动回到 host
    │
    ├─ set_vcpu_pcpu(id, -1)
    │
    ├─ A::save_guest_ctx(vcpu)      // x86: no-op; arm/rv: 保存 sysregs
    │
    └─ A::exit_handler(vcpu) → match {
        ├─ Resume/VmSkip → continue (回到 loop 顶部)
        ├─ VmExit        → break Ok(())   // guest 正常退出 (VMCALL_DONE)
        ├─ VmAbort       → break Ok(())
        └─ Exit          → break Err(())  // 未处理的 exit reason
    }
  }

vCPU Exit & 资源释放

vmm_run_vcpu 返回前:
│
├─ A::teardown_vcpu(vcpu)
│   ├─ x86: vmclear(vmcs_pa)       // 使 VMCS 进入 "clear" 状态，可安全释放
│   └─ arm/rv: no-op
│
└─ 函数返回 → 调用者 (spawn 闭包或 smp 闭包)

线程结束 → vcpu drop:
│
├─ vcpu.hw_pages: Vec<GlobalPage> drop
│   ├─ VMCS page (x86) → GlobalPage::drop → dealloc_pages
│   ├─ guest code page (arm/rv)
│   └─ guest stack page
│
├─ vcpu.vm: Arc<VmShared> refcount--
│   └─ 最后一个 Arc drop 时 → VmShared drop:
│       └─ guest_mem: Option<A::GuestMem> drop
│           ├─ Ept::drop   → destroy(): 遍历 PML4→PDPT→PD 释放所有子页
│           ├─ Stage2::drop → destroy(): 遍历 L1→L2 释放
│           └─ GStage::drop → destroy(): 遍历 root→L1 释放
│
└─ 线程退出，ktask 回收

关键点：
- 所有页面通过 GlobalPage RAII 跟随 owner 生命周期释放
- x86 VMCS 释放前必须 vmclear（teardown_vcpu 保证）
- 页表树只存根页，Drop 时从根向下遍历 free 子页
- 唯一的 mem::forget：VMXON region（CPU 级永久）