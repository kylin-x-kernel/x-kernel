// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Guest physical memory access for virtual device backends.

use alloc::sync::Arc;
use core::sync::atomic::{Ordering, fence};

use vdev_core::{DmaError, GuestDma};

use crate::{arch::VmmArch, mm::GuestMem, vm::VmRef};

const PAGE_SIZE: u64 = 4096;

pub struct VmGuestDma<A: VmmArch> {
    vm: VmRef<A>,
}

impl<A: VmmArch> VmGuestDma<A> {
    pub fn new(vm: VmRef<A>) -> Self {
        Self { vm }
    }

    fn copy_chunks(
        &self,
        mut gpa: u64,
        mut len: usize,
        mut copy: impl FnMut(usize, *mut u8) -> Result<(), DmaError>,
    ) -> Result<(), DmaError> {
        let guest_mem = self.vm.guest_mem().ok_or(DmaError::NoGuestMem)?;
        let mut done = 0usize;
        while len != 0 {
            let hpa = guest_mem.gpa_to_hpa(gpa).ok_or(DmaError::AddressFault)?;
            let page_left = (PAGE_SIZE - (gpa & (PAGE_SIZE - 1))) as usize;
            let chunk = len.min(page_left);
            let va = kaddr_layout::p2v(hpa as usize) as *mut u8;
            copy(done, va)?;
            done = done.checked_add(chunk).ok_or(DmaError::RangeOverflow)?;
            gpa = gpa
                .checked_add(chunk as u64)
                .ok_or(DmaError::RangeOverflow)?;
            len -= chunk;
        }
        Ok(())
    }
}

impl<A: VmmArch> GuestDma for VmGuestDma<A> {
    fn read(&self, gpa: u64, buf: &mut [u8]) -> Result<(), DmaError> {
        fence(Ordering::Acquire);
        self.copy_chunks(gpa, buf.len(), |done, src| {
            let remaining = buf.len() - done;
            let page_left = PAGE_SIZE as usize - ((src as usize) & (PAGE_SIZE as usize - 1));
            let chunk = remaining.min(page_left);
            // SAFETY: `copy_chunks` translated the current GPA and bounded this
            // copy to the current page, so the source range is mapped guest RAM.
            unsafe {
                core::ptr::copy_nonoverlapping(src as *const u8, buf[done..].as_mut_ptr(), chunk)
            };
            Ok(())
        })
    }

    fn write(&self, gpa: u64, buf: &[u8]) -> Result<(), DmaError> {
        self.copy_chunks(gpa, buf.len(), |done, dst| {
            let remaining = buf.len() - done;
            let page_left = PAGE_SIZE as usize - ((dst as usize) & (PAGE_SIZE as usize - 1));
            let chunk = remaining.min(page_left);
            // SAFETY: `copy_chunks` translated the current GPA and bounded this
            // copy to the current page, so the destination range is mapped guest RAM.
            unsafe { core::ptr::copy_nonoverlapping(buf[done..].as_ptr(), dst, chunk) };
            Ok(())
        })?;
        fence(Ordering::Release);
        Ok(())
    }
}

pub fn make_guest_dma<A: VmmArch + 'static>(vm: VmRef<A>) -> Arc<dyn GuestDma> {
    Arc::new(VmGuestDma::new(vm))
}
