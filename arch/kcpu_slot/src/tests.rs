// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

#[cfg(test)]
mod tests {
    use core::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    cpu_slot! {
        static TEST_SLOT: usize = 0;
    }

    cpu_slot_cell! {
        static TEST_CELL: usize = 0;
    }

    /// A pin that reports a fixed logical CPU and an explicit slot-area base,
    /// for host-side tests. The dynamic accessors only need the CPU identity;
    /// the static accessors read `base()`, so a caller-owned area is supplied.
    struct TestPin {
        cpu: CpuId,
        base: usize,
    }
    // SAFETY: The test never migrates; the reported CPU and base are whatever
    // the caller chose for the duration of the test.
    unsafe impl PinCurrentCpu for TestPin {
        fn current_cpu(&self) -> CpuId {
            self.cpu
        }

        fn base(&self) -> usize {
            self.base
        }
    }

    #[repr(align(64))]
    struct Backing([u8; 256]);

    #[test]
    fn dynamic_slot_initializes_each_cpu_and_reads_remotely() {
        let mut backing = Backing([0; 256]);
        // SAFETY: `backing` is aligned, writable, and outlives the chunk and
        // dynamic handle; two 128-byte CPU areas fit in the backing store.
        let chunk =
            unsafe { CpuSlotChunk::from_raw_parts(backing.0.as_mut_ptr(), 2, 128).unwrap() };
        let slot = chunk
            .alloc::<usize, ()>(|cpu| Ok(cpu.as_usize() + 10))
            .unwrap();
        let second = chunk
            .alloc::<u32, ()>(|cpu| Ok((cpu.as_usize() + 20) as u32))
            .unwrap();
        assert_eq!(slot.get_remote(CpuId::new(0)), Some(&10));
        assert_eq!(slot.get_remote(CpuId::new(1)), Some(&11));
        assert_eq!(slot.get_remote(CpuId::new(2)), None);
        assert_eq!(second.get_remote(CpuId::new(0)), Some(&20));
        assert_eq!(second.get_remote(CpuId::new(1)), Some(&21));

        // The dynamic handle uses the CPU identity carried by the pin; the
        // architecture base is unrelated to this caller-owned chunk.
        let pin = TestPin {
            cpu: CpuId::new(1),
            base: 0,
        };
        // SAFETY: CPU 1 was initialized by `alloc` and the pin is pinned to it.
        assert_eq!(unsafe { *slot.get(&pin).unwrap() }, 11);
    }

    #[test]
    fn static_slot_reads_and_writes_via_pin_base() {
        // SAFETY: `from_offset_fn(0)` is valid only for a base that points at
        // the slot's own cell; the test pins `base` to the area being accessed.
        let slot = unsafe { CpuSlot::<usize>::from_offset_fn(|| 0) };
        let cell = unsafe { CpuSlotCell::<usize>::from_offset_fn(|| 0) };
        // SAFETY: The array is unique, aligned, initialized, and outlives the
        // pin and the accesses below.
        let mut area = [0usize; 8];
        let pin = TestPin {
            cpu: CpuId::new(0),
            base: area.as_mut_ptr() as usize,
        };
        // SAFETY: `area` is an initialized, writable CPU area and the pin is pinned.
        unsafe {
            *slot.get_mut(&pin) = 42;
        }
        // SAFETY: `area` remains valid and exclusively owned for this test.
        let got = unsafe { slot.get(&pin) };
        assert_eq!(*got, 42);
        // SAFETY: The cell is initialized and accessed through the same pin.
        unsafe {
            cell.write(&pin, 7);
        }
        // SAFETY: See above.
        assert_eq!(unsafe { cell.read(&pin) }, 7);
    }

    #[test]
    fn chunk_reports_misalignment_separately() {
        let mut backing = Backing([0; 256]);
        // SAFETY: `backing` outlives the chunk; the stride below is not a
        // multiple of 8 so the u64 allocation must be rejected as Misaligned.
        let chunk =
            unsafe { CpuSlotChunk::from_raw_parts(backing.0.as_mut_ptr(), 2, 100).unwrap() };
        let result = chunk.alloc::<u64, ()>(|_| Ok(0u64));
        assert!(matches!(result, Err(SlotInitError::Misaligned)));
    }

    #[test]
    fn chunk_rejects_invalid_backing_arguments() {
        let mut backing = Backing([0; 256]);
        // SAFETY: Null and zero-sized arguments are rejected before any
        // memory access; the non-null pointer refers to the local backing.
        assert!(unsafe { CpuSlotChunk::from_raw_parts(core::ptr::null_mut(), 2, 128) }.is_none());
        assert!(unsafe { CpuSlotChunk::from_raw_parts(backing.0.as_mut_ptr(), 0, 128) }.is_none());
        assert!(unsafe { CpuSlotChunk::from_raw_parts(backing.0.as_mut_ptr(), 2, 0) }.is_none());
    }

    static DROPS: AtomicUsize = AtomicUsize::new(0);

    struct DropProbe;
    impl Drop for DropProbe {
        fn drop(&mut self) {
            DROPS.fetch_add(1, Ordering::Relaxed);
        }
    }

    #[test]
    fn failed_dynamic_initialization_rolls_back() {
        DROPS.store(0, Ordering::Relaxed);
        let mut backing = Backing([0; 256]);
        // SAFETY: The backing provides two exclusive CPU areas and outlives
        // the chunk and failed allocation.
        let chunk =
            unsafe { CpuSlotChunk::from_raw_parts(backing.0.as_mut_ptr(), 2, 128).unwrap() };
        let result = chunk.alloc::<DropProbe, usize>(|cpu| {
            if cpu.as_usize() == 1 {
                Err(1)
            } else {
                Ok(DropProbe)
            }
        });
        assert!(matches!(result, Err(SlotInitError::Init(1))));
        assert_eq!(DROPS.load(Ordering::Relaxed), 1);
    }
}
