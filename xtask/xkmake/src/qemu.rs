// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use std::{
    env, fs, io,
    net::{TcpListener, UdpSocket},
    os::{fd::AsRawFd, unix::fs::FileTypeExt},
    path::{Path, PathBuf},
    process::Command,
};

use xconfig::build_config::{KernelArch, VirtioBus};

use crate::{
    build::{BootArtifacts, Bundle},
    cli::{RunArgs, X86BootMode},
    error::{Error, IoResultExt, Result},
    process::Process,
};

pub(crate) fn run(bundle: &Bundle, args: &RunArgs) -> Result<()> {
    let context = &bundle.context;
    if context.config.arch() != KernelArch::X86_64
        && (args.boot.is_some() || args.ovmf_code.is_some() || args.ovmf_vars_template.is_some())
    {
        return Err(Error::Message(
            "--boot and OVMF options are only valid for x86_64".to_string(),
        ));
    }
    let qemu_program = qemu_program(context.config.arch());
    let mut command = Process::new(qemu_program, context.dry_run, context.verbosity);
    command
        .current_dir(&context.workspace_root)
        .arg("-m")
        .arg(&args.memory)
        .arg("-smp")
        .arg(
            args.smp
                .unwrap_or_else(|| context.config.nr_cpus())
                .to_string(),
        );

    let accel = selected_accel(
        context.config.arch(),
        args.no_accel,
        context.config.is_enabled("KFEAT_VMM"),
    );
    add_platform_args(&mut command, bundle, args, qemu_program, accel)?;
    add_devices(&mut command, bundle, args, qemu_program)?;
    if !args.graphic {
        command.arg("-nographic");
    }
    command.args(args.qemu_args.iter());

    println!(
        "Running {} ({})",
        context.config.platform(),
        context.config.target()
    );
    // In verbose/dry-run mode `command.run()` prints the command itself, so
    // suppress it here to avoid a duplicate; otherwise show the command once so
    // the user can see exactly what is being launched.
    if context.verbosity == 0 && !context.dry_run {
        println!("{}", command.command_lines());
    }

    if context.dry_run {
        return command.run();
    }
    // Mirror QEMU output to the terminal and keep a copy so a panic
    // backtrace can be symbolicated after the run (`symbolize::auto`).
    command.run_tee(log_path(bundle))
}

/// Where the QEMU output log is mirrored for post-run symbolication.
pub(crate) fn log_path(bundle: &Bundle) -> PathBuf {
    bundle.directory.join("qemu.log")
}

fn qemu_program(arch: KernelArch) -> &'static str {
    match arch {
        KernelArch::Aarch64 => "qemu-system-aarch64",
        KernelArch::Riscv64 => "qemu-system-riscv64",
        KernelArch::X86_64 => "qemu-system-x86_64",
        KernelArch::LoongArch64 => "qemu-system-loongarch64",
    }
}

/// Select the QEMU accelerator backend.
///
/// VMM-enabled kernels need architectural virtualization support inside the
/// guest. Most CI hosts expose KVM to the outer QEMU but do not support nested
/// virtualization, so run those kernels under TCG unless acceleration is
/// explicitly disabled already.
fn selected_accel(guest: KernelArch, no_accel: bool, vmm: bool) -> Option<&'static str> {
    if no_accel || vmm {
        None
    } else {
        hardware_accel(guest)
    }
}

/// Hardware acceleration backend (KVM on Linux, HVF on macOS) available when
/// the host can run the guest arch natively. Returns the QEMU accelerator
/// backend name, or `None` when only software emulation (TCG) is usable.
fn hardware_accel(guest: KernelArch) -> Option<&'static str> {
    let arch_matches = matches!(
        (env::consts::ARCH, guest),
        ("x86_64", KernelArch::X86_64)
            | ("aarch64", KernelArch::Aarch64)
            | ("riscv64", KernelArch::Riscv64)
    );
    if !arch_matches {
        return None;
    }
    match env::consts::OS {
        "macos" => Some("hvf"),
        "linux" if !is_wsl() && is_character_device(Path::new("/dev/kvm")) => Some("kvm"),
        _ => None,
    }
}

fn is_character_device(path: &Path) -> bool {
    fs::metadata(path).is_ok_and(|metadata| metadata.file_type().is_char_device())
}

/// Whether the Linux host is really WSL/WSL2, where KVM is unavailable.
fn is_wsl() -> bool {
    fs::read_to_string("/proc/sys/kernel/osrelease")
        .is_ok_and(|release| release.to_ascii_lowercase().contains("microsoft"))
}

fn add_platform_args(
    command: &mut Process,
    bundle: &Bundle,
    args: &RunArgs,
    qemu_program: &str,
    accel: Option<&str>,
) -> Result<()> {
    let context = &bundle.context;
    match context.config.platform() {
        "kplat-aarch64" => {
            let machine = if context.config.is_enabled("KFEAT_VMM") {
                "virt,virtualization=on"
            } else {
                "virt,gic-version=3"
            };
            let cpu = if accel.is_some() {
                "host"
            } else {
                "cortex-a76"
            };
            command
                .args(["-cpu", cpu, "-machine", machine, "-kernel"])
                .arg(direct_kernel(bundle)?);
            if let Some(backend) = accel {
                command.args(["-accel", backend]);
            }
        }
        "kplat-riscv64" => {
            command
                .args(["-machine", "virt", "-bios", "default", "-kernel"])
                .arg(direct_kernel(bundle)?);
            if let Some(backend) = accel {
                command.args(["-cpu", "host", "-accel", backend]);
            }
        }
        "kplat-loongarch64" => {
            command
                .args(["-machine", "virt", "-kernel"])
                .arg(direct_kernel(bundle)?);
        }
        "kplat-x86_64" => {
            add_x86_boot_args(command, bundle, args, qemu_program, accel)?;
        }
        platform => {
            return Err(Error::Message(format!(
                "platform {platform} has no QEMU run strategy"
            )));
        }
    }
    Ok(())
}

fn direct_kernel(bundle: &Bundle) -> Result<&std::path::Path> {
    match &bundle.boot_artifacts {
        BootArtifacts::Direct { kernel_bin } => Ok(kernel_bin),
        BootArtifacts::X86 { .. } => Err(Error::Message(
            "x86 boot artifacts cannot be used by a direct-boot platform".to_string(),
        )),
    }
}

fn add_x86_boot_args(
    command: &mut Process,
    bundle: &Bundle,
    args: &RunArgs,
    qemu_program: &str,
    accel: Option<&str>,
) -> Result<()> {
    let BootArtifacts::X86 {
        linuxboot_image,
        uefi_image,
    } = &bundle.boot_artifacts
    else {
        return Err(Error::Message(
            "x86_64 platform requires x86 boot artifacts".to_string(),
        ));
    };

    let vmm = bundle.context.config.is_enabled("KFEAT_VMM");
    match args.boot.unwrap_or(X86BootMode::Linuxboot) {
        X86BootMode::Linuxboot => {
            command
                .args(["-machine", "q35", "-kernel"])
                .arg(linuxboot_image);
            if let Some(backend) = accel {
                command.args(["-cpu", "host", "-accel", backend]);
            } else if vmm {
                command.args(["-cpu", "max,+vmx"]);
            }
            // Otherwise no `-cpu`; QEMU uses its default — unchanged.
        }
        X86BootMode::Uefi => {
            let firmware = resolve_ovmf(bundle, args, qemu_program)?;
            let cpu = if accel.is_some() {
                "host"
            } else if vmm {
                "max,+vmx"
            } else {
                "max"
            };
            command
                .args(["-cpu", cpu, "-machine", "q35"])
                .arg("-drive")
                .arg(format!(
                    "if=pflash,format=raw,unit=0,file={},readonly=on",
                    firmware.code.display()
                ))
                .arg("-drive")
                .arg(format!(
                    "if=pflash,format=raw,unit=1,file={}",
                    firmware.vars.display()
                ))
                .arg("-drive")
                .arg(format!(
                    "if=ide,format=raw,index=0,file={}",
                    uefi_image.display()
                ));
            if let Some(backend) = accel {
                command.args(["-accel", backend]);
            }
        }
    }
    Ok(())
}

struct OvmfFirmware {
    code: PathBuf,
    vars: PathBuf,
}

fn resolve_ovmf(bundle: &Bundle, args: &RunArgs, qemu_program: &str) -> Result<OvmfFirmware> {
    let code = resolve_firmware_path(
        &bundle.context.workspace_root,
        args.ovmf_code.as_deref(),
        "OVMF_CODE",
        &ovmf_code_candidates(qemu_program),
    )?;
    let vars_template = resolve_firmware_path(
        &bundle.context.workspace_root,
        args.ovmf_vars_template.as_deref(),
        "OVMF_VARS_TEMPLATE",
        &ovmf_vars_candidates(qemu_program),
    )?;
    let runtime_dir = bundle
        .context
        .target_dir
        .join("xkmake/runtime")
        .join(bundle.context.config.platform());
    let vars = runtime_dir.join("OVMF_VARS.fd");
    if !bundle.context.dry_run {
        fs::create_dir_all(&runtime_dir).with_path(&runtime_dir)?;
        fs::copy(&vars_template, &vars).with_path(&vars)?;
    }
    Ok(OvmfFirmware { code, vars })
}

fn resolve_firmware_path(
    workspace_root: &std::path::Path,
    explicit: Option<&std::path::Path>,
    environment_name: &str,
    candidates: &[PathBuf],
) -> Result<PathBuf> {
    let requested = explicit
        .map(PathBuf::from)
        .or_else(|| env::var_os(environment_name).map(PathBuf::from));
    if let Some(path) = requested {
        let path = if path.is_absolute() {
            path
        } else {
            workspace_root.join(path)
        };
        if path.is_file() {
            return Ok(path);
        }
        return Err(Error::Message(format!(
            "{environment_name} firmware file not found: {}",
            path.display()
        )));
    }

    candidates
        .iter()
        .find(|path| path.is_file())
        .cloned()
        .ok_or_else(|| {
            Error::Message(format!(
                "{environment_name} firmware not found; pass the corresponding --{} option",
                environment_name.to_ascii_lowercase().replace('_', "-")
            ))
        })
}

fn ovmf_code_candidates(qemu_program: &str) -> Vec<PathBuf> {
    let mut candidates = vec![
        PathBuf::from("/usr/share/OVMF/OVMF_CODE_4M.fd"),
        PathBuf::from("/usr/share/OVMF/OVMF_CODE.fd"),
        PathBuf::from("/usr/share/edk2/x64/OVMF_CODE.fd"),
        PathBuf::from("/opt/homebrew/share/qemu/edk2-x86_64-code.fd"),
    ];
    if let Some(directory) = qemu_data_directory(qemu_program) {
        candidates.push(directory.join("edk2-x86_64-code.fd"));
    }
    candidates
}

fn ovmf_vars_candidates(qemu_program: &str) -> Vec<PathBuf> {
    let mut candidates = vec![
        PathBuf::from("/usr/share/OVMF/OVMF_VARS_4M.fd"),
        PathBuf::from("/usr/share/OVMF/OVMF_VARS.fd"),
        PathBuf::from("/usr/share/edk2/x64/OVMF_VARS.fd"),
        PathBuf::from("/opt/homebrew/share/qemu/edk2-i386-vars.fd"),
    ];
    if let Some(directory) = qemu_data_directory(qemu_program) {
        candidates.push(directory.join("edk2-i386-vars.fd"));
    }
    candidates
}

fn qemu_data_directory(qemu_program: &str) -> Option<PathBuf> {
    let program = env::split_paths(&env::var_os("PATH")?)
        .map(|directory| directory.join(qemu_program))
        .find(|path| path.is_file())?;
    Some(program.parent()?.parent()?.join("share/qemu"))
}

/// Scan width for host forwarding ports: with the preferred port at 61005,
/// the candidates run through 62005 (1000 ports).
const HOSTFWD_PORT_PROBE_SPAN: u16 = 1000;

/// Step between host forwarding port candidates (61005, 61015, ...).
const HOSTFWD_PORT_PROBE_STEP: usize = 10;

/// Scan width for vsock guest CIDs: with the preferred CID at 103, the
/// candidates run through 203 (100 CIDs).
const VSOCK_CID_PROBE_SPAN: u32 = 100;

/// `VHOST_VSOCK_SET_GUEST_CID` ioctl request code (`_IOW(VHOST_VIRTIO, 0x60,
/// u64)`), used to probe whether a guest CID is already claimed on the host.
/// The Linux `libc` bindings do not export this constant, so it is written
/// out with its derivation.
const VHOST_VSOCK_SET_GUEST_CID: libc::c_ulong = 0x4008_af60;

/// Pick the host port forwarded to guest TCP/UDP port 5555.
///
/// Starts at the preferred `--hostfwd-port` and, when it is busy, scans
/// sequentially upward (step `HOSTFWD_PORT_PROBE_STEP`) for the first port
/// free on both TCP and UDP. Probing binds to `0.0.0.0`, mirroring the
/// wildcard bind QEMU's `user` netdev performs for `hostfwd=tcp::PORT-:5555`.
fn resolve_hostfwd_port(args: &RunArgs) -> Result<u16> {
    let preferred = args.hostfwd_port;
    let last = preferred.saturating_add(HOSTFWD_PORT_PROBE_SPAN);
    for port in (preferred..=last).step_by(HOSTFWD_PORT_PROBE_STEP) {
        if is_host_port_free(port)? {
            return Ok(port);
        }
    }
    Err(Error::Message(format!(
        "no free host port in {preferred}..={last} for guest TCP/UDP port 5555"
    )))
}

/// Whether nothing listens on `port` on any address, for either TCP or UDP.
///
/// Only a busy port (`AddrInUse`) counts as unavailable. Any other bind
/// error (for example `EACCES` when a non-root user picks a port below
/// 1024) is a real problem QEMU would hit too, so it is reported instead of
/// being misdiagnosed as a busy port and silently skipped.
fn is_host_port_free(port: u16) -> Result<bool> {
    match TcpListener::bind(("0.0.0.0", port)) {
        Ok(_) => {}
        Err(error) if error.kind() == io::ErrorKind::AddrInUse => return Ok(false),
        Err(error) => {
            return Err(Error::Message(format!(
                "cannot bind TCP port {port} to check forwarding: {error}"
            )));
        }
    }
    match UdpSocket::bind(("0.0.0.0", port)) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == io::ErrorKind::AddrInUse => Ok(false),
        Err(error) => Err(Error::Message(format!(
            "cannot bind UDP port {port} to check forwarding: {error}"
        ))),
    }
}

/// Pick a free guest CID for the vhost-vsock device.
///
/// Starts at the preferred `--vsock-cid` and, when it is already claimed by
/// another guest on the host, scans sequentially upward for the first free
/// one. Availability is probed with `VHOST_VSOCK_SET_GUEST_CID` on a
/// temporary `/dev/vhost-vsock` fd: the kernel answers `EADDRINUSE` when the
/// CID is taken. The fd is dropped right after the probe, releasing any CID
/// it claimed. When the device is missing or cannot be probed (for example
/// without permission), the preferred CID is used unchanged; a kernel
/// `EINVAL` (a reserved CID) is reported as an error instead of falling
/// back to the same illegal value.
fn resolve_vsock_cid(args: &RunArgs) -> Result<u32> {
    let preferred = args.vsock_cid;
    let fd = match fs::File::open("/dev/vhost-vsock") {
        Ok(fd) => fd,
        Err(error) => {
            log::warn!(
                "cannot open /dev/vhost-vsock to probe CID availability ({error}); \
                 using the preferred CID {preferred}"
            );
            return Ok(preferred);
        }
    };
    let last = preferred.saturating_add(VSOCK_CID_PROBE_SPAN);
    for candidate in preferred..=last {
        // The kernel reads a u64 guest CID for this request.
        let cid = u64::from(candidate);
        // SAFETY: `fd` is a valid open file descriptor, and the kernel only
        // reads the CID value from the provided pointer for this request.
        let rc = unsafe { libc::ioctl(fd.as_raw_fd(), VHOST_VSOCK_SET_GUEST_CID, &cid) };
        if rc == 0 {
            return Ok(candidate);
        }
        let error = io::Error::last_os_error();
        match error.raw_os_error() {
            Some(libc::EADDRINUSE) => {}
            Some(libc::EINVAL) => {
                return Err(Error::Message(format!(
                    "the kernel rejected vsock guest CID {candidate}: {error}"
                )));
            }
            _ => {
                log::warn!(
                    "cannot probe vsock CID availability ({error}); using the preferred CID {preferred}"
                );
                return Ok(preferred);
            }
        }
    }
    Err(Error::Message(format!(
        "no free vsock guest CID in {preferred}..={last}; \
         release one of them or pick another with --vsock-cid"
    )))
}

fn add_devices(
    command: &mut Process,
    bundle: &Bundle,
    args: &RunArgs,
    qemu_program: &str,
) -> Result<()> {
    let config = &bundle.context.config;
    let needs_block = config.is_enabled("KFEAT_DRIVER_VIRTIO_BLK") && !args.no_block;
    let needs_net = config.is_enabled("KFEAT_DRIVER_VIRTIO_NET") && !args.no_net;
    let needs_graphic = config.is_enabled("KFEAT_DRIVER_VIRTIO_GPU") && args.graphic;
    let wants_vsock = config.is_enabled("KFEAT_DRIVER_VIRTIO_SOCKET") && !args.no_vsock;
    let needs_rng = config.is_enabled("KFEAT_DRIVER_VIRTIO_RNG");

    let suffix = if needs_block || needs_net || needs_graphic || wants_vsock || needs_rng {
        virtio_device_suffix(config.virtio_bus())?
    } else {
        ""
    };

    if needs_block {
        let disk_image = bundle.context.workspace_root.join(&args.disk_image);
        if !bundle.context.dry_run && !disk_image.is_file() {
            return Err(Error::Message(format!(
                "disk image not found: {}",
                disk_image.display()
            )));
        }
        command
            .arg("-device")
            .arg(format!("virtio-blk-{suffix},drive=disk0"))
            .arg("-drive")
            .arg(format!(
                "id=disk0,if=none,format=raw,file={}",
                disk_image.display()
            ));
    }

    if needs_net {
        // Probing binds sockets, so in dry-run mode (which only prints the
        // command without side effects) the preferred port is used as-is.
        let hostfwd_port = if bundle.context.dry_run {
            args.hostfwd_port
        } else {
            resolve_hostfwd_port(args)?
        };
        if hostfwd_port != args.hostfwd_port {
            log::info!(
                "host port {} is busy; forwarding guest TCP/UDP port 5555 on host port {}",
                args.hostfwd_port,
                hostfwd_port
            );
        }
        command
            .arg("-device")
            .arg(format!("virtio-net-{suffix},netdev=net0"))
            .arg("-netdev")
            .arg(format!(
                "user,id=net0,hostfwd=tcp::{hostfwd_port}-:5555,hostfwd=udp::{hostfwd_port}-:5555"
            ));
    }

    if needs_graphic {
        command
            .arg("-device")
            .arg(format!("virtio-gpu-{suffix}"))
            .args(["-vga", "none", "-serial", "mon:stdio"]);
    }

    if wants_vsock {
        let device = format!("vhost-vsock-{suffix}");
        if qemu_supports_device(qemu_program, &device) {
            // Probing claims CIDs on /dev/vhost-vsock, so it is skipped in
            // dry-run mode and the preferred CID is used as-is.
            let vsock_cid = if bundle.context.dry_run {
                args.vsock_cid
            } else {
                resolve_vsock_cid(args)?
            };
            if vsock_cid != args.vsock_cid {
                log::info!(
                    "vsock CID {} is busy; using guest CID {}",
                    args.vsock_cid,
                    vsock_cid
                );
            }
            command.arg("-device").arg(format!(
                "{device},id=virtiosocket0,guest-cid={vsock_cid}"
            ));
        }
    }

    if needs_rng {
        command
            .args(["-object", "rng-random,id=host_rng0"])
            .arg("-device")
            .arg(format!("virtio-rng-{suffix},rng=host_rng0"));
    }

    Ok(())
}

fn qemu_supports_device(qemu_program: &str, device: &str) -> bool {
    let output = match Command::new(qemu_program)
        .args(["-device", "help"])
        .output()
    {
        Ok(output) => output,
        Err(error) => {
            log::warn!(
                "cannot probe {qemu_program} for {device} support: {error}; skipping vsock"
            );
            return false;
        }
    };
    if !output.status.success() {
        log::warn!(
            "{qemu_program} device probe exited with {}; skipping vsock",
            output.status
        );
        return false;
    }

    let is_supported = output_contains_device(&output.stdout, device)
        || output_contains_device(&output.stderr, device);
    if !is_supported {
        log::warn!(
            "{qemu_program} does not support {device}; continuing without a vsock device"
        );
    }
    is_supported
}

fn output_contains_device(output: &[u8], device: &str) -> bool {
    String::from_utf8_lossy(output)
        .lines()
        .any(|line| line.contains(device))
}

fn virtio_device_suffix(bus: Option<VirtioBus>) -> Result<&'static str> {
    match bus {
        Some(VirtioBus::Mmio) => Ok("device"),
        Some(VirtioBus::Pci) => Ok("pci"),
        None => Err(Error::Message(
            "the kernel enables virtio devices but no virtio bus is configured".to_string(),
        )),
    }
}

#[cfg(test)]
mod tests {
    use std::net::{TcpListener, UdpSocket};

    use xconfig::build_config::KernelArch;

    use super::{is_host_port_free, output_contains_device, selected_accel};

    #[test]
    fn device_probe_matches_qemu_device_listing() {
        let output = br#"name \"virtio-net-pci\"
name \"vhost-vsock-pci\", bus PCI
"#;

        assert!(output_contains_device(output, "vhost-vsock-pci"));
        assert!(!output_contains_device(output, "vhost-vsock-device"));
    }

    #[test]
    fn vmm_forces_tcg_even_when_accel_is_available() {
        let guest = match std::env::consts::ARCH {
            "aarch64" => KernelArch::Aarch64,
            "riscv64" => KernelArch::Riscv64,
            _ => KernelArch::X86_64,
        };

        assert_eq!(selected_accel(guest, false, true), None);
    }

    #[test]
    fn is_host_port_free_accepts_an_unused_port() {
        // Port 0 asks the kernel for an ephemeral port, which is always free.
        assert!(is_host_port_free(0).unwrap());
    }

    #[test]
    fn is_host_port_free_detects_occupied_tcp_port() {
        let listener = TcpListener::bind(("0.0.0.0", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        assert!(!is_host_port_free(port).unwrap());
    }

    #[test]
    fn is_host_port_free_detects_occupied_udp_port() {
        let socket = UdpSocket::bind(("0.0.0.0", 0)).unwrap();
        let port = socket.local_addr().unwrap().port();
        assert!(!is_host_port_free(port).unwrap());
    }
}
