// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

#![no_std]

extern crate alloc;

use alloc::{format, sync::Arc, vec, vec::Vec};
use core::{
    marker::PhantomData,
    mem::{MaybeUninit, size_of},
    ops::{Deref, DerefMut},
    sync::atomic::{AtomicU32, AtomicUsize, Ordering},
};

use kcred::initial_cred;
use kerrno::{KError, KResult};
use khal::{mem::v2p, paging::MappingFlags};
use kidentity::PidHandle;
use kprocess::{AsThread, LiveAddressSpace, Pid, Thread, build_process_thread, start_user_task};
use ksignal::api::SignalActions;
use ksync::{Mutex, spin::SpinNoIrq};
use ktask::{TaskInner, current};
use memaddr::{PAGE_SIZE_4K, VirtAddr};
use osvm::{read_vm_mem, write_vm_mem};
use unittest::{TestDescriptor, TestResult};

static NEXT_TEST_USER_ADDR: AtomicUsize = AtomicUsize::new(kaddr_layout::USER_HEAP_BASE);
static NEXT_TEST_PROCESS_ID: AtomicU32 = AtomicU32::new(1_000_000);
static INIT_TEST_THREAD_HOOK: SpinNoIrq<Option<InitTestThreadHook>> = SpinNoIrq::new(None);

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

/// Optional test-thread initialization hook used when installing the unittest runtime.
pub type InitTestThreadHook = fn(&Thread);

fn registered_init_test_thread(thread: &Thread) {
    if let Some(init_thread) = *INIT_TEST_THREAD_HOOK.lock() {
        init_thread(thread);
    }
}

/// Runs a test in a newly constructed user task.
fn run_in_user_task(test: &TestDescriptor, init_thread: InitTestThreadHook) -> TestResult {
    let pid = NEXT_TEST_PROCESS_ID.fetch_add(1, Ordering::Relaxed) as Pid;
    let mut aspace = memspace::MmSpace::new_user_empty().expect("user test address space");
    ksignal::map_signal_trampoline(&mut aspace).expect("user test signal trampoline");
    // On x86_64 the scheduler switches page tables purely from the `cr3` saved
    // in each task's context; unlike aarch64 there is no per-switch runtime hook
    // that re-derives the root. A new task defaults to the kernel page table, so
    // the user mapping at `USER_HEAP_BASE` would otherwise be unreachable and
    // fault. Capture the root before `aspace` moves into the thread and install
    // it on the task, mirroring the init-process and clone paths.
    let page_table_root = aspace.page_table_hw_root();
    let task_number = PidHandle::fixed_root(pid);
    let process = kprocess::Process::new_init_with_task_number(task_number.clone());
    let thread = build_process_thread(
        process,
        task_number.clone(),
        "[unittest-user]".into(),
        Arc::new(vec![]),
        Arc::new(Mutex::new(aspace)),
        fs_context::copy_init_fs_struct(),
        Arc::new(SpinNoIrq::new(SignalActions::default())),
        initial_cred(),
    );
    init_thread(&thread);

    let result = Arc::new(SpinNoIrq::new(None));
    let result_ref = result.clone();
    let test = *test;
    let mut task = TaskInner::new_user(
        move || {
            let outcome = (test.test_fn)();
            *result_ref.lock() = Some(outcome);
        },
        format!("unittest-user-{pid}"),
        task_number,
        thread,
    );
    task.ctx_mut().set_page_table_root(page_table_root);
    start_user_task(task).join();
    result.lock().take().unwrap_or(TestResult::Failed)
}

/// Register the shared unittest runtime using a crate-provided test-thread initialization hook.
pub fn register_unittest_runtime(init_thread: InitTestThreadHook) {
    *INIT_TEST_THREAD_HOOK.lock() = Some(init_thread);
    unittest::register_user_test_executor(run_registered_test_user_task);
}

fn run_registered_test_user_task(test: &TestDescriptor) -> TestResult {
    run_in_user_task(test, registered_init_test_thread)
}

pub struct TestUserBuffer {
    aspace: LiveAddressSpace,
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
        let aspace = thread
            .process()
            .address_space()
            .map_err(|_| KError::BadState)?;
        let mapped_size = len.max(1).next_multiple_of(PAGE_SIZE_4K);
        let num_pages = mapped_size / PAGE_SIZE_4K;
        let kernel_va = kalloc::global_allocator()
            .alloc_pages(num_pages, PAGE_SIZE_4K, kalloc::UsageKind::VirtMem)
            .map_err(|_| KError::NoMemory)?;

        // SAFETY: `kernel_va` names a freshly allocated kernel mapping of
        // `mapped_size` bytes that may be zero-initialized in place.
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
        // SAFETY: `MaybeUninit<u8>` has the same layout as `u8`. `out` owns a
        // valid `len`-byte buffer, so reborrowing its backing storage as
        // `MaybeUninit<u8>` is sound for filling it via `read_vm_mem`.
        let out_uninit = unsafe {
            core::slice::from_raw_parts_mut(out.as_mut_ptr().cast::<MaybeUninit<u8>>(), len)
        };
        read_vm_mem(self.user_addr as *const u8, out_uninit).map_err(KError::from)?;
        Ok(out)
    }

    pub fn write_u64(&self, value: u64) -> KResult {
        write_vm_mem(self.user_addr as *mut u64, core::slice::from_ref(&value)).map_err(Into::into)
    }

    pub fn read_u64(&self) -> KResult<u64> {
        let mut out = 0u64;
        // SAFETY: `out` is a live `u64`, so its storage may be reborrowed as a
        // single-element `MaybeUninit<u64>` slice for `read_vm_mem`.
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
        // SAFETY: `user_addr..user_addr + len` lies within the mapped test buffer
        // and `&mut self` guarantees exclusive access.
        unsafe { core::slice::from_raw_parts_mut(self.user_addr as *mut u8, len) }
    }

    pub fn as_user_ref<T>(&mut self) -> &mut T {
        assert!(size_of::<T>() <= self.len);
        // SAFETY: the mapped test buffer is at least `size_of::<T>()` bytes and
        // `&mut self` guarantees exclusive access to that region.
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
        // SAFETY: the mapped test buffer contains a previously initialized `T`
        // value written through the same typed view.
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
        // SAFETY: the mapped buffer is sized for `[T; N]` and `&mut self`
        // guarantees exclusive access to that region.
        unsafe { core::slice::from_raw_parts_mut(self.as_user_ptr(), N) }
    }

    pub fn as_user_ref(&mut self) -> &mut [T; N] {
        // SAFETY: the mapped buffer is sized for `[T; N]` and `&mut self`
        // guarantees exclusive access to that region.
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
        // SAFETY: the mapped buffer contains a previously initialized `[T; N]`
        // value written through the same typed view.
        unsafe { (self.as_user_ptr() as *const [T; N]).read() }
    }
}

impl<T, const N: usize> Deref for TestUserArray<T, N> {
    type Target = [T];

    fn deref(&self) -> &Self::Target {
        // SAFETY: the mapped buffer is sized for `N` elements and remains valid
        // for shared access for the lifetime of `self`.
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
