// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use kcpu_id_map::{LogicalCpuId, raw_cpu_id};
use kerrno::{KError, KErrorKind, KResult};
use kplat::sys::SysCtrl;
struct PowerImpl;
#[impl_dev_interface]
impl SysCtrl for PowerImpl {
    #[cfg(feature = "smp")]
    fn boot_ap(logical_cpu_id: LogicalCpuId, stack_top_paddr: usize) -> KResult {
        use khal::mem::{v2p, va};
        if sbi_rt::probe_extension(sbi_rt::Hsm).is_unavailable() {
            warn!("HSM SBI extension is not supported for current SEE.");
            return Err(KErrorKind::OperationNotSupported.into());
        }
        let entry = v2p(va!(
            kernel_boot::arch::_start_secondary as *const () as usize
        ));
        let raw_cpu_id = raw_cpu_id(logical_cpu_id).unwrap_or_else(|| {
            panic!(
                "missing raw CPU id mapping for logical CPU {}",
                logical_cpu_id.as_usize()
            )
        });
        sbi_rt::hart_start(raw_cpu_id.as_usize(), entry.as_usize(), stack_top_paddr).map_err(
            |err| {
                warn!(
                    "failed to boot hart {} via SBI HSM: {err:?}",
                    raw_cpu_id.as_usize()
                );
                KError::from(KErrorKind::Io)
            },
        )?;
        Ok(())
    }

    fn shutdown() -> ! {
        info!("Shutting down...");
        sbi_rt::system_reset(sbi_rt::Shutdown, sbi_rt::NoReason);
        warn!("It should shutdown!");
        loop {
            karch::stop_cpu();
        }
    }
}
