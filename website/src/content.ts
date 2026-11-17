export type Lang = "en" | "zh";

export type Status = "implemented" | "partial";

export interface Feature {
  icon: "abi" | "arch" | "tee" | "gpu" | "crate" | "verify";
  title: string;
  body: string;
  tag: string;
}

export interface Layer {
  id: string;
  name: string;
  summary: string;
  crates: string[];
}

export interface Capability {
  area: string;
  status: Status;
  scope: string;
  source: string;
}

export interface Workload {
  id: "systemd" | "docker" | "desktop";
  label: string;
  title: string;
  body: string;
  command: string;
  output: string[];
  image?: string;
  caption?: string;
}

const shared = {
  stats: [
    { value: 4, suffix: "", key: "arch" },
    { value: 200, suffix: "+", key: "crates" },
    { value: 280, suffix: "+", key: "syscalls" },
    { value: 700, suffix: "K+", key: "loc" },
  ],
  archs: [
    { id: "x86_64", name: "x86_64", ci: true, isa: "CISC · 64-bit", color: "#22d3ee" },
    { id: "aarch64", name: "AArch64", ci: true, isa: "ARMv8-A", color: "#a855f7" },
    { id: "riscv64", name: "RISC-V 64", ci: true, isa: "RV64GC", color: "#f59e0b" },
    { id: "loongarch64", name: "LoongArch64", ci: false, isa: "LA64", color: "#34d399" },
  ],
  platforms: [
    "QEMU x86_64",
    "QEMU AArch64",
    "QEMU RISC-V 64",
    "QEMU LoongArch64",
    "Hygon CSV",
    "AArch64 virtCCA",
    "AArch64 crosvm",
    "RK3588 · Orange Pi 5 Plus",
    "Kylin-x pKVM",
  ],
  // Hero slogan "Build the X": each group flashes its burst words, then holds on `land`.
  sloganGroups: [
    { burst: ["Server", "Laptop", "Kiosk", "Tablet", "Terminal", "Workstation"], land: "Desktop" },
    { burst: ["Sandbox", "Vault", "Keystore", "Shield", "Secret", "TEE"], land: "Enclave" },
    { burst: ["Pod", "Image", "Runtime", "Cluster", "Service", "Microservice"], land: "Container" },
    { burst: ["Edge", "Cloud", "Car", "Robot", "Satellite", "AI Agent"], land: "Future" },
  ],
  teeOrbit: ["SM2", "ECC", "RSA", "RustCrypto", "HUK", "Attestation", "TA", "Secure storage"],
  links: {
    repo: "https://gitee.com/openkylin/x-kernel",
    issues: "https://gitee.com/openkylin/x-kernel/issues",
    pulls: "https://gitee.com/openkylin/x-kernel/pulls",
    chat: "https://x-kernel.zulipchat.com",
    harness: "https://gitee.com/openkylin/starry-test-harness",
    agentos: "https://gitee.com/openkylin/x-agentos",
    starry: "https://github.com/Starry-OS/StarryOS",
    arceos: "https://github.com/arceos-org/arceos",
  },
  quickstart: [
    {
      id: "aarch64",
      label: "AArch64",
      lines: [
        "cp platforms/kplat-aarch64/qemu_defconfig .config",
        "make defconfig",
        "make rootfs",
        "make build",
        "make run",
      ],
    },
    {
      id: "x86_64",
      label: "x86_64",
      lines: [
        "cp platforms/kplat-x86_64/qemu_defconfig .config",
        "make defconfig",
        "make run            # LinuxBoot",
        "make run UEFI=y     # OVMF / UEFI",
      ],
    },
    {
      id: "riscv64",
      label: "RISC-V 64",
      lines: [
        "cp platforms/kplat-riscv64/qemu_defconfig .config",
        "make defconfig",
        "make rootfs",
        "make run",
      ],
    },
    {
      id: "desktop",
      label: "Desktop",
      lines: [
        "cp platforms/kplat-x86_64/qemu_defconfig .config",
        "make defconfig",
        "make run GRAPHIC=y GPU_XRES=1920 GPU_YRES=1080 \\",
        "         DISK_IMG=<desktop-image>",
      ],
    },
  ],
  bootLog: [
    { t: "0.000000", tag: "boot", msg: "X-Kernel v0.1.0-2606 · aarch64 · smp=4" },
    { t: "0.000412", tag: "khal", msg: "exception vectors installed, MMU on" },
    { t: "0.001083", tag: "mm", msg: "memspace: kernel address space ready" },
    { t: "0.002217", tag: "kirq", msg: "irq domain online · GICv3" },
    { t: "0.003904", tag: "sched", msg: "EEVDF scheduler started on 4 CPUs" },
    { t: "0.006531", tag: "drv", msg: "virtio: blk net gpu input console" },
    { t: "0.009120", tag: "vfs", msg: "mounted ext4 on / · procfs sysfs devfs" },
    { t: "0.011876", tag: "tee", msg: "TA loader ready · secure storage up" },
    { t: "0.014002", tag: "exec", msg: "exec /sbin/init → systemd" },
    { t: "1.208311", tag: "init", msg: "systemd[1]: Reached target graphical.target" },
  ],
} as const;

const en = {
  nav: {
    features: "Features",
    architecture: "Architecture",
    workloads: "Workloads",
    tee: "TEE",
    capabilities: "Status",
    start: "Quick start",
    repo: "Source",
  },
  hero: {
    badge: "v0.1.0-2606 · first release",
    slogan: {
      build: "Build the",
      tagline: "Unlimited possibilities.",
    },
    rotatePrefix: "Boot unmodified",
    rotate: ["systemd", "Docker", "Chromium", "niri", "Xorg", "glibc", "musl"],
    rotateSuffix: "on a memory-safe monolithic kernel.",
    lead:
      "X-Kernel targets the Linux ABI across four CPU architectures, ships in-kernel confidential computing, and drives real Wayland/X11 desktops — built from 200+ crates with explicit ownership boundaries.",
    ctaPrimary: "Get started",
    ctaSecondary: "View on Gitee",
    stats: {
      arch: "CPU architectures",
      crates: "crates by contract",
      syscalls: "syscall entries",
      loc: "lines of Rust",
    },
    scroll: "scroll",
  },
  features: {
    kicker: "Why X-Kernel",
    title: "Not a syscall toy. A kernel for real userspace.",
    lead:
      "End-to-end workloads — from PID 1 to a compositor — are what distinguish X-Kernel from a syscall-only Linux ABI implementation.",
    items: [
      {
        icon: "abi",
        tag: "ksyscall · posix",
        title: "Linux ABI, production userspace",
        body:
          "A Linux-compatible syscall and POSIX surface. systemd boots as PID 1 with glibc, Docker hello-world runs, and Wayland/X11 desktops run on the same kernel.",
      },
      {
        icon: "arch",
        tag: "x86_64 · AArch64 · RV64 · LA64",
        title: "Four architectures, one kernel",
        body:
          "x86_64, AArch64, RISC-V 64 and LoongArch64 share one HAL contract. x86_64, AArch64 and RISC-V are continuously built and tested.",
      },
      {
        icon: "tee",
        tag: "CSV · virtCCA · pKVM",
        title: "Confidential computing, in-kernel",
        body:
          "Hygon CSV with hardware-rooted key derivation and attestation, ARM virtCCA HUK derivation, TA lifecycle, secure storage and SM2/ECC/RSA via RustCrypto.",
      },
      {
        icon: "gpu",
        tag: "DRM/KMS · virtio-gpu",
        title: "Desktop-ready graphics",
        body:
          "DRM/KMS, virtio-gpu 2D/3D, EDID, page-flip/vblank, fbdev and virtio-input drive Chromium, niri and Xorg/Xwayland — including host-window resize.",
      },
      {
        icon: "crate",
        tag: "memspace · kuaccess · kexec · kirq",
        title: "Componentized by contract",
        body:
          "Monolithic at runtime, modular at compile time. Each crate owns its data, invariants and lifecycle, with design.md and security.md next to the code.",
      },
      {
        icon: "verify",
        tag: "unittest · Harness · LTP",
        title: "Linux-baselined verification",
        body:
          "In-kernel unit tests, the Starry Test Harness with LTP, Open POSIX and libc-test, plus TEE suites — and every unsafe block carries a SAFETY: proof.",
      },
    ] as Feature[],
  },
  architecture: {
    kicker: "Design",
    title: "Monolithic at runtime. Componentized by contract.",
    lead:
      "Crate boundaries are compile-time API contracts, not privilege boundaries — so a reviewer can scope every change to its owner and its downstream users.",
    hint: "Select a layer to inspect it",
    crateLabel: "Representative crates",
    layers: [
      {
        id: "user",
        name: "Linux userspace",
        summary: "Unmodified binaries built for musl or glibc: systemd, Docker, Chromium, niri, Xorg/Xwayland.",
        crates: ["systemd", "docker", "chromium", "niri", "xorg", "busybox"],
      },
      {
        id: "abi",
        name: "ABI",
        summary: "Syscall dispatch, POSIX object semantics and the TEE entry points that userspace talks to.",
        crates: ["ksyscall", "posix/*", "linux_sysno", "kseccomp", "kbpf", "tee_kernel"],
      },
      {
        id: "subsys",
        name: "Subsystems",
        summary: "Memory, processes, scheduling, VFS, networking and drivers — each its own ownership domain.",
        crates: ["memspace", "pagecache", "kprocess", "kexec", "ksched", "kvfs", "knet", "kdriver"],
      },
      {
        id: "hal",
        name: "Arch HAL",
        summary: "CPU, exceptions, paging, timers and IPI behind one contract, plus kirq as the common interrupt core.",
        crates: ["khal", "karch", "kcpu", "kirq", "kipi", "page_table", "x86-apic"],
      },
      {
        id: "plat",
        name: "Platforms",
        summary: "Boot handoff and platform glue for emulated, virtualized and confidential targets.",
        crates: ["kplat-x86_64", "kplat-aarch64", "kplat-riscv64", "kplat-loongarch64", "x86_64-uefi-loader"],
      },
    ] as Layer[],
    pillars: [
      { title: "API-first ownership", body: "memspace, kuaccess, kexec and kirq are separate contracts — not internals of a catch-all crate." },
      { title: "Audit boundaries", body: "Crates carry design.md and security.md, so threat models live beside the code they describe." },
      { title: "Unsafe boundaries", body: "MMIO, DMA, context switch and TEE glue stay behind subsystem APIs, each unsafe justified." },
    ],
  },
  platforms: {
    kicker: "Platforms",
    title: "From QEMU to silicon to confidential VMs.",
    ci: "CI",
    targets: "Supported targets",
  },
  workloads: {
    kicker: "Real workloads",
    title: "See it run.",
    lead: "Three end-to-end workloads that exercise the whole stack — not just individual syscalls.",
    items: [
      {
        id: "systemd",
        label: "systemd + glibc",
        title: "systemd as PID 1",
        body: "systemd 257 and glibc 2.41 on a Debian 13 rootfs reach a login shell on X-Kernel (x86_64).",
        command: "make rootfs ROOTFS_VARIANT=debian-systemd && make run",
        output: ["Welcome to Debian GNU/Linux 13 (trixie)!", "[  OK  ] Reached target multi-user.target", "debian login: _"],
        image: "/images/systemd-glibc.png",
        caption: "systemd 257 + glibc 2.41 · Debian 13 · x86_64",
      },
      {
        id: "docker",
        label: "Docker",
        title: "A container guest",
        body: "The Docker smoke workload runs hello-world with host networking. User-namespace and cgroup gaps are tracked openly.",
        command: "docker run --network=host --rm hello-world",
        output: [
          "Unable to find image 'hello-world:latest' locally",
          "latest: Pulling from library/hello-world",
          "Status: Downloaded newer image for hello-world:latest",
          "",
          "Hello from Docker!",
          "This message shows that your installation appears to be working correctly.",
        ],
      },
      {
        id: "desktop",
        label: "Wayland desktop",
        title: "A full desktop session",
        body: "niri + Chromium + PCManFM on virtio-gpu at 1600×900, with keyboard, mouse and host-window resize.",
        command: "make run GRAPHIC=y DISK_IMG=<desktop-image>",
        output: ["drm: virtio-gpu scanout 0 · 1600x900", "input: virtio keyboard, virtio mouse", "niri: compositor started"],
        image: "/images/desktop-niri-bilibili-fm.png",
        caption: "niri + Chromium + PCManFM · x86_64 · virtio-gpu",
      },
    ] as Workload[],
  },
  tee: {
    kicker: "Confidential computing",
    title: "Trust rooted in hardware, enforced in-kernel.",
    lead:
      "TEE support provides the runtime isolation for trusted applications and guest workloads, with hardware-rooted keys on confidential-computing platforms.",
    platforms: [
      { name: "Hygon CSV", state: "HUK + attestation", body: "Hardware-rooted key derivation and remote attestation for x86_64 confidential guests." },
      { name: "ARM virtCCA", state: "HUK derivation", body: "Hardware unique key derivation for AArch64 confidential guests." },
      { name: "pKVM", state: "experimental", body: "Kylin-x pKVM guest mode behind dedicated feature flags." },
    ],
    caps: ["TA lifecycle", "Signed TA loading", "Multi-process secure storage", "SM2", "ECC", "RSA", "RustCrypto"],
  },
  capabilities: {
    kicker: "Capability status",
    title: "Honest about what works.",
    lead: "Implemented means present and exercised by a test or workload. Partial means present but covering a known subset.",
    implemented: "Implemented",
    partial: "Partial",
    items: [
      { area: "Memory management", status: "implemented", scope: "Address spaces, faults, COW, mmap/brk", source: "mm/" },
      { area: "Process & scheduling", status: "implemented", scope: "fork/exec, wait*, signals, futex, timers, EEVDF", source: "process/ task/" },
      { area: "VFS & path resolution", status: "implemented", scope: "namei, mount traversal, openat2 RESOLVE_*", source: "fs/kvfs" },
      { area: "Drivers & device model", status: "implemented", scope: "Typed driver core, virtio, PCI, IRQ", source: "drivers/ io/" },
      { area: "Desktop workloads", status: "implemented", scope: "Chromium, niri, Xorg/Xwayland, input, resize", source: "io/drmdevice" },
      { area: "TEE", status: "implemented", scope: "TA lifecycle, secure storage, SM2/ECC/RSA", source: "tee/" },
      { area: "ext4", status: "partial", scope: "Read/write and project quota", source: "fs/filesystems/kext4" },
      { area: "Other filesystems", status: "partial", scope: "tmpfs, procfs, sysfs, devpts, 9p, fat, cgroup2, bpffs", source: "fs/filesystems/" },
      { area: "Networking", status: "partial", scope: "TCP/IP, Unix, netfilter, vsock, netlink, TIPC", source: "net/knet" },
      { area: "Graphics (kernel)", status: "partial", scope: "DRM/KMS, virtio-gpu 2D/3D, EDID, fbdev", source: "io/drmdevice" },
      { area: "Container guest", status: "partial", scope: "docker hello-world smoke", source: "uapps/mini-oci" },
    ] as Capability[],
  },
  verify: {
    kicker: "Safety & verification",
    title: "Memory safety is the default. Unsafe must justify itself.",
    lead:
      "Rust removes whole classes of kernel bugs. What remains is fenced in: every unsafe block needs a SAFETY: justification, and dylint rules enforce lock, API and user-ABI discipline in CI.",
    layers: [
      { name: "In-kernel unit tests", cmd: "make unittest", body: "Run inside QEMU with coverage reports." },
      { name: "Starry Test Harness", cmd: "make ci-test", body: "LTP, Open POSIX, libc-test, fs, net, desktop and container cases." },
      { name: "TEE suites", cmd: "tee tests", body: "Secure storage and crypto regression tests." },
    ],
  },
  start: {
    kicker: "Quick start",
    title: "From clone to boot in five commands.",
    lead: "Configuration is Kconfig-driven. Pick a platform defconfig, expand it, and run under QEMU.",
    copy: "Copy",
    copied: "Copied",
  },
  footer: {
    tagline: "A monolithic Rust kernel targeting the Linux ABI.",
    community: "Community",
    project: "Project",
    repo: "Repository",
    issues: "Issues",
    pulls: "Pull requests",
    chat: "Zulip chat",
    harness: "Test Harness",
    agentos: "Desktop image",
    license: "Licensed under Apache-2.0.",
    thanks: "Inspired by",
    and: "and",
  },
};

export type Content = typeof en;

const zh: Content = {
  nav: {
    features: "特性",
    architecture: "架构",
    workloads: "实际负载",
    tee: "机密计算",
    capabilities: "能力现状",
    start: "快速开始",
    repo: "源码",
  },
  hero: {
    badge: "v0.1.0-2606 · 首个正式版本",
    slogan: {
      build: "Build the",
      tagline: "创造无限可能",
    },
    rotatePrefix: "无需修改即可运行",
    rotate: ["systemd", "Docker", "Chromium", "niri", "Xorg", "glibc", "musl"],
    rotateSuffix: "—— 运行在内存安全的宏内核之上。",
    lead:
      "X-Kernel 面向 Linux ABI，覆盖四种 CPU 架构，内置机密计算能力，能够驱动真实的 Wayland/X11 桌面 —— 由 200 多个边界清晰的 crate 构成。",
    ctaPrimary: "快速开始",
    ctaSecondary: "前往 Gitee",
    stats: {
      arch: "种 CPU 架构",
      crates: "个契约化 crate",
      syscalls: "个系统调用入口",
      loc: "行 Rust 代码",
    },
    scroll: "向下滚动",
  },
  features: {
    kicker: "为什么是 X-Kernel",
    title: "不是系统调用玩具，而是承载真实用户态的内核。",
    lead: "从 PID 1 到桌面合成器的端到端负载，正是 X-Kernel 与只实现系统调用的 Linux ABI 项目的区别。",
    items: [
      {
        icon: "abi",
        tag: "ksyscall · posix",
        title: "Linux ABI，生产级用户态",
        body: "兼容 Linux 的系统调用与 POSIX 接口。systemd 以 glibc 作为 PID 1 启动，Docker hello-world 可运行，Wayland/X11 桌面跑在同一个内核上。",
      },
      {
        icon: "arch",
        tag: "x86_64 · AArch64 · RV64 · LA64",
        title: "四种架构，一个内核",
        body: "x86_64、AArch64、RISC-V 64 与 LoongArch64 共享同一套 HAL 契约，其中 x86_64、AArch64、RISC-V 持续集成构建与测试。",
      },
      {
        icon: "tee",
        tag: "CSV · virtCCA · pKVM",
        title: "内核内建机密计算",
        body: "海光 CSV 硬件根密钥派生与远程证明，ARM virtCCA HUK 派生，TA 生命周期、安全存储，以及基于 RustCrypto 的 SM2/ECC/RSA。",
      },
      {
        icon: "gpu",
        tag: "DRM/KMS · virtio-gpu",
        title: "面向桌面的图形栈",
        body: "DRM/KMS、virtio-gpu 2D/3D、EDID、page-flip/vblank、fbdev 与 virtio-input，支撑 Chromium、niri 和 Xorg/Xwayland，并支持宿主窗口缩放。",
      },
      {
        icon: "crate",
        tag: "memspace · kuaccess · kexec · kirq",
        title: "以契约组件化",
        body: "运行时是宏内核，编译期是组件化。每个 crate 拥有自己的数据、不变式与生命周期，design.md 和 security.md 紧贴代码。",
      },
      {
        icon: "verify",
        tag: "unittest · Harness · LTP",
        title: "以 Linux 为基线的验证",
        body: "内核单元测试、包含 LTP / Open POSIX / libc-test 的 Starry Test Harness 与 TEE 测试套件 —— 每个 unsafe 块都附带 SAFETY: 说明。",
      },
    ],
  },
  architecture: {
    kicker: "设计",
    title: "运行时是宏内核，组件按契约划分。",
    lead: "crate 边界是编译期的 API 契约而非特权边界 —— 评审者可以把每次改动限定在它的所有者与下游使用者之内。",
    hint: "点击某一层查看详情",
    crateLabel: "代表性 crate",
    layers: [
      {
        id: "user",
        name: "Linux 用户态",
        summary: "为 musl 或 glibc 构建的原生二进制：systemd、Docker、Chromium、niri、Xorg/Xwayland。",
        crates: ["systemd", "docker", "chromium", "niri", "xorg", "busybox"],
      },
      {
        id: "abi",
        name: "ABI 层",
        summary: "系统调用分发、POSIX 对象语义，以及用户态访问的 TEE 入口。",
        crates: ["ksyscall", "posix/*", "linux_sysno", "kseccomp", "kbpf", "tee_kernel"],
      },
      {
        id: "subsys",
        name: "子系统",
        summary: "内存、进程、调度、VFS、网络与驱动 —— 各自是独立的所有权域。",
        crates: ["memspace", "pagecache", "kprocess", "kexec", "ksched", "kvfs", "knet", "kdriver"],
      },
      {
        id: "hal",
        name: "架构 HAL",
        summary: "CPU、异常、分页、定时器与 IPI 统一在一个契约之后，kirq 作为通用中断核心。",
        crates: ["khal", "karch", "kcpu", "kirq", "kipi", "page_table", "x86-apic"],
      },
      {
        id: "plat",
        name: "平台层",
        summary: "面向仿真、虚拟化与机密计算目标的启动交接与平台胶水代码。",
        crates: ["kplat-x86_64", "kplat-aarch64", "kplat-riscv64", "kplat-loongarch64", "x86_64-uefi-loader"],
      },
    ],
    pillars: [
      { title: "API 优先的所有权", body: "memspace、kuaccess、kexec 与 kirq 是独立契约，而不是某个大杂烩 crate 的内部实现。" },
      { title: "可审计边界", body: "crate 自带 design.md 与 security.md，威胁模型与它描述的代码放在一起。" },
      { title: "unsafe 边界", body: "MMIO、DMA、上下文切换与 TEE 胶水都封装在子系统 API 之后，每处 unsafe 都有说明。" },
    ],
  },
  platforms: {
    kicker: "平台",
    title: "从 QEMU 到真实硬件，再到机密虚拟机。",
    ci: "CI",
    targets: "支持的目标平台",
  },
  workloads: {
    kicker: "实际负载",
    title: "眼见为实。",
    lead: "三个覆盖整个技术栈的端到端负载 —— 而不只是单个系统调用。",
    items: [
      {
        id: "systemd",
        label: "systemd + glibc",
        title: "systemd 作为 PID 1",
        body: "Debian 13 rootfs 上的 systemd 257 与 glibc 2.41 在 X-Kernel（x86_64）上启动到登录 shell。",
        command: "make rootfs ROOTFS_VARIANT=debian-systemd && make run",
        output: ["Welcome to Debian GNU/Linux 13 (trixie)!", "[  OK  ] Reached target multi-user.target", "debian login: _"],
        image: "/images/systemd-glibc.png",
        caption: "systemd 257 + glibc 2.41 · Debian 13 · x86_64",
      },
      {
        id: "docker",
        label: "Docker",
        title: "容器宿主",
        body: "Docker 冒烟负载以 host 网络运行 hello-world。user namespace 与 cgroup 的差距会公开跟踪。",
        command: "docker run --network=host --rm hello-world",
        output: [
          "Unable to find image 'hello-world:latest' locally",
          "latest: Pulling from library/hello-world",
          "Status: Downloaded newer image for hello-world:latest",
          "",
          "Hello from Docker!",
          "This message shows that your installation appears to be working correctly.",
        ],
      },
      {
        id: "desktop",
        label: "Wayland 桌面",
        title: "完整的桌面会话",
        body: "virtio-gpu 1600×900 下运行 niri + Chromium + PCManFM，支持键盘、鼠标与宿主窗口缩放。",
        command: "make run GRAPHIC=y DISK_IMG=<desktop-image>",
        output: ["drm: virtio-gpu scanout 0 · 1600x900", "input: virtio keyboard, virtio mouse", "niri: compositor started"],
        image: "/images/desktop-niri-bilibili-fm.png",
        caption: "niri + Chromium（哔哩哔哩）+ PCManFM · x86_64 · virtio-gpu",
      },
    ],
  },
  tee: {
    kicker: "机密计算",
    title: "信任根植于硬件，隔离由内核保障。",
    lead: "TEE 为可信应用与客户机负载提供运行时隔离，并在机密计算平台上使用硬件根密钥。",
    platforms: [
      { name: "海光 CSV", state: "HUK + 远程证明", body: "面向 x86_64 机密客户机的硬件根密钥派生与远程证明。" },
      { name: "ARM virtCCA", state: "HUK 派生", body: "面向 AArch64 机密客户机的硬件唯一密钥派生。" },
      { name: "pKVM", state: "实验性", body: "通过专用特性开关启用的麒麟 pKVM 客户机模式。" },
    ],
    caps: ["TA 生命周期", "TA 签名加载", "多进程安全存储", "SM2", "ECC", "RSA", "RustCrypto"],
  },
  capabilities: {
    kicker: "能力现状",
    title: "坦诚标注每一项能力。",
    lead: "「已实现」表示已存在并被测试或负载覆盖；「部分实现」表示已存在但只覆盖已知子集。",
    implemented: "已实现",
    partial: "部分实现",
    items: [
      { area: "内存管理", status: "implemented", scope: "地址空间、缺页、COW、mmap/brk", source: "mm/" },
      { area: "进程与调度", status: "implemented", scope: "fork/exec、wait*、信号、futex、定时器、EEVDF", source: "process/ task/" },
      { area: "VFS 与路径解析", status: "implemented", scope: "namei、挂载点遍历、openat2 RESOLVE_*", source: "fs/kvfs" },
      { area: "驱动与设备模型", status: "implemented", scope: "类型化驱动核心、virtio、PCI、IRQ", source: "drivers/ io/" },
      { area: "桌面负载", status: "implemented", scope: "Chromium、niri、Xorg/Xwayland、输入、缩放", source: "io/drmdevice" },
      { area: "TEE", status: "implemented", scope: "TA 生命周期、安全存储、SM2/ECC/RSA", source: "tee/" },
      { area: "ext4", status: "partial", scope: "读写与项目配额", source: "fs/filesystems/kext4" },
      { area: "其他文件系统", status: "partial", scope: "tmpfs、procfs、sysfs、devpts、9p、fat、cgroup2、bpffs", source: "fs/filesystems/" },
      { area: "网络", status: "partial", scope: "TCP/IP、Unix、netfilter、vsock、netlink、TIPC", source: "net/knet" },
      { area: "图形（内核侧）", status: "partial", scope: "DRM/KMS、virtio-gpu 2D/3D、EDID、fbdev", source: "io/drmdevice" },
      { area: "容器宿主", status: "partial", scope: "docker hello-world 冒烟测试", source: "uapps/mini-oci" },
    ],
  },
  verify: {
    kicker: "安全与验证",
    title: "内存安全是默认值，unsafe 必须自证。",
    lead: "Rust 消除了整类内核缺陷，剩下的部分被严格围起来：每个 unsafe 块都要有 SAFETY: 说明，dylint 规则在 CI 中强制执行锁、API 与用户 ABI 约束。",
    layers: [
      { name: "内核单元测试", cmd: "make unittest", body: "在 QEMU 中运行并生成覆盖率报告。" },
      { name: "Starry Test Harness", cmd: "make ci-test", body: "LTP、Open POSIX、libc-test、文件系统、网络、桌面与容器用例。" },
      { name: "TEE 测试", cmd: "tee tests", body: "安全存储与密码算法回归测试。" },
    ],
  },
  start: {
    kicker: "快速开始",
    title: "从克隆到启动，只需五条命令。",
    lead: "配置由 Kconfig 驱动：选一个平台 defconfig，展开后在 QEMU 中运行。",
    copy: "复制",
    copied: "已复制",
  },
  footer: {
    tagline: "面向 Linux ABI 的 Rust 宏内核。",
    community: "社区",
    project: "项目",
    repo: "代码仓库",
    issues: "问题反馈",
    pulls: "合并请求",
    chat: "Zulip 讨论区",
    harness: "测试套件",
    agentos: "桌面镜像",
    license: "基于 Apache-2.0 协议开源。",
    thanks: "灵感来自",
    and: "与",
  },
};

export const content: Record<Lang, Content> = { en, zh };
export { shared };
