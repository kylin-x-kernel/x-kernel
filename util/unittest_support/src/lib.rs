// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

#![no_std]
#![allow(missing_docs)]

extern crate alloc;

use alloc::{sync::Arc, vec, vec::Vec};
use core::{
    marker::PhantomData,
    mem::{MaybeUninit, size_of, transmute},
    ops::{Deref, DerefMut},
    sync::atomic::{AtomicUsize, Ordering},
};

use kerrno::{KError, KResult};
use khal::{mem::v2p, paging::MappingFlags};
use ksync::Mutex;
use ktask::current;
use kthread::AsThread;
use memaddr::{PAGE_SIZE_4K, VirtAddr};
use osvm::{read_vm_mem, write_vm_mem};

static NEXT_TEST_USER_ADDR: AtomicUsize = AtomicUsize::new(kaddr_layout::USER_HEAP_BASE);

#[macro_export]
macro_rules! __unittest_support_user_vec {
    ($value:expr; $len:expr) => {{
        $crate::TestUserArray::from_array([$value; $len]).unwrap()
    }};
    ($($value:expr),+ $(,)?) => {{
        $crate::TestUserArray::from_array([$($value),+]).unwrap()
    }};
}

pub use crate::__unittest_support_user_vec as user_vec;

pub struct TestUserBuffer {
    aspace: Arc<Mutex<memspace::AddrSpace>>,
    user_addr: usize,
    mapped_size: usize,
    kernel_va: usize,
    num_pages: usize,
    len: usize,
}

impl TestUserBuffer {
    pub fn new(len: usize) -> KResult<Self> {
        let current_task = current();
        let thread = current_task.try_as_thread().ok_or(KError::BadState)?;
        let aspace = thread.proc_state.address_space().clone();
        let mapped_size = len.max(1).next_multiple_of(PAGE_SIZE_4K);
        let num_pages = mapped_size / PAGE_SIZE_4K;
        let kernel_va = kalloc::global_allocator()
            .alloc_pages(num_pages, PAGE_SIZE_4K, kalloc::UsageKind::VirtMem)
            .map_err(|_| KError::NoMemory)?;

        unsafe {
            core::ptr::write_bytes(kernel_va as *mut u8, 0, mapped_size);
        }

        let user_addr = NEXT_TEST_USER_ADDR.fetch_add(mapped_size, Ordering::Relaxed);
        aspace
            .lock()
            .map_linear(
                VirtAddr::from_usize(user_addr),
                v2p(VirtAddr::from_usize(kernel_va)),
                mapped_size,
                MappingFlags::READ | MappingFlags::WRITE | MappingFlags::USER,
            )
            .map_err(|_| {
                kalloc::global_allocator().dealloc_pages(
                    kernel_va,
                    num_pages,
                    kalloc::UsageKind::VirtMem,
                );
                KError::NoMemory
            })?;

        Ok(Self {
            aspace,
            user_addr,
            mapped_size,
            kernel_va,
            num_pages,
            len,
        })
    }

    pub fn write_bytes(&self, data: &[u8]) -> KResult {
        if data.len() > self.len {
            return Err(KError::InvalidInput);
        }
        write_vm_mem(self.user_addr as *mut u8, data).map_err(Into::into)
    }

    pub fn read_bytes(&self, len: usize) -> KResult<Vec<u8>> {
        if len > self.len {
            return Err(KError::InvalidInput);
        }
        let mut out = vec![0u8; len];
        read_vm_mem(self.user_addr as *const u8, unsafe {
            transmute::<&mut [u8], &mut [MaybeUninit<u8>]>(&mut out[..])
        })
        .map_err(KError::from)?;
        Ok(out)
    }

    pub fn write_u64(&self, value: u64) -> KResult {
        write_vm_mem(self.user_addr as *mut u64, core::slice::from_ref(&value)).map_err(Into::into)
    }

    pub fn read_u64(&self) -> KResult<u64> {
        let mut out = 0u64;
        let out_slice = unsafe {
            core::slice::from_raw_parts_mut((&mut out as *mut u64).cast::<MaybeUninit<u64>>(), 1)
        };
        read_vm_mem(self.user_addr as *const u64, out_slice).map_err(KError::from)?;
        Ok(out)
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub fn as_user_ptr<T>(&self) -> *mut T {
        assert!(size_of::<T>() <= self.len);
        self.user_addr as *mut T
    }

    pub fn as_user_slice(&mut self, len: usize) -> &mut [u8] {
        assert!(len <= self.len);
        unsafe { core::slice::from_raw_parts_mut(self.user_addr as *mut u8, len) }
    }

    pub fn as_user_ref<T>(&mut self) -> &mut T {
        assert!(size_of::<T>() <= self.len);
        unsafe { &mut *(self.user_addr as *mut T) }
    }
}

pub struct TestUserValue<T> {
    buffer: TestUserBuffer,
    _marker: PhantomData<T>,
}

impl<T> TestUserValue<T> {
    pub fn new() -> KResult<Self> {
        Ok(Self {
            buffer: TestUserBuffer::new(size_of::<T>())?,
            _marker: PhantomData,
        })
    }

    pub fn from_value(value: T) -> KResult<Self>
    where
        T: Copy,
    {
        let mut user_value = Self::new()?;
        user_value.write(value);
        Ok(user_value)
    }

    pub fn as_user_ref(&mut self) -> &mut T {
        self.buffer.as_user_ref::<T>()
    }

    pub fn as_user_ptr(&self) -> *mut T {
        self.buffer.as_user_ptr::<T>()
    }

    pub fn write(&mut self, value: T)
    where
        T: Copy,
    {
        *self.as_user_ref() = value;
    }

    pub fn read(&self) -> T
    where
        T: Copy,
    {
        unsafe { self.as_user_ptr().read() }
    }
}

pub struct TestUserArray<T, const N: usize> {
    buffer: TestUserBuffer,
    _marker: PhantomData<T>,
}

impl<T, const N: usize> TestUserArray<T, N> {
    pub fn new() -> KResult<Self> {
        Ok(Self {
            buffer: TestUserBuffer::new(size_of::<[T; N]>())?,
            _marker: PhantomData,
        })
    }

    pub fn from_array(value: [T; N]) -> KResult<Self>
    where
        T: Copy,
    {
        let mut user_array = Self::new()?;
        user_array.write(value);
        Ok(user_array)
    }

    pub fn len(&self) -> usize {
        N
    }

    pub fn is_empty(&self) -> bool {
        N == 0
    }

    pub fn as_user_slice(&mut self) -> &mut [T] {
        unsafe { core::slice::from_raw_parts_mut(self.as_user_ptr(), N) }
    }

    pub fn as_user_ref(&mut self) -> &mut [T; N] {
        unsafe { &mut *(self.as_user_ptr() as *mut [T; N]) }
    }

    pub fn as_user_ptr(&self) -> *mut T {
        self.buffer.as_user_ptr::<T>()
    }

    pub fn write(&mut self, value: [T; N])
    where
        T: Copy,
    {
        self.as_user_slice().copy_from_slice(&value);
    }

    pub fn read(&self) -> [T; N]
    where
        T: Copy,
    {
        unsafe { (self.as_user_ptr() as *const [T; N]).read() }
    }
}

impl<T, const N: usize> Deref for TestUserArray<T, N> {
    type Target = [T];

    fn deref(&self) -> &Self::Target {
        unsafe { core::slice::from_raw_parts(self.as_user_ptr(), N) }
    }
}

impl<T, const N: usize> DerefMut for TestUserArray<T, N> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.as_user_slice()
    }
}

impl Drop for TestUserBuffer {
    fn drop(&mut self) {
        let _ = self
            .aspace
            .lock()
            .unmap(VirtAddr::from_usize(self.user_addr), self.mapped_size);
        kalloc::global_allocator().dealloc_pages(
            self.kernel_va,
            self.num_pages,
            kalloc::UsageKind::VirtMem,
        );
    }
}
