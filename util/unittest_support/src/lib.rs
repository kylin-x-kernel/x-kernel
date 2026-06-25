// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

#![no_std]
#![allow(missing_docs)]

#[macro_use]
extern crate klogger;
extern crate alloc;

use alloc::{sync::Arc, vec, vec::Vec};
use core::{
    marker::PhantomData,
    mem::{MaybeUninit, size_of},
    ops::{Deref, DerefMut},
    sync::atomic::{AtomicUsize, Ordering},
};

use kcred::Credentials;
use kerrno::{KError, KResult};
use khal::{mem::v2p, paging::MappingFlags};
use kprocess::Pid;
use ksignal::api::SignalActions;
use ksync::{
    Mutex,
    spin::{NoPreempt, SpinNoIrq},
};
use ktask::{KTaskExt, TaskExt, current};
use kthread::{AsThread, ProcessState, ProcessStateConfig, Thread};
use memaddr::{PAGE_SIZE_4K, VirtAddr};
use osvm::{read_vm_mem, write_vm_mem};
use unittest::{TestDescriptor, TestResult};

static NEXT_TEST_USER_ADDR: AtomicUsize = AtomicUsize::new(kaddr_layout::USER_HEAP_BASE);
static NEXT_TEST_PROCESS_ID: AtomicUsize = AtomicUsize::new(0x7000_0000);
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

fn alloc_test_process_id() -> Pid {
    NEXT_TEST_PROCESS_ID.fetch_add(1, Ordering::Relaxed) as Pid
}

fn current_task_ptr() -> *mut ktask::TaskInner {
    let current_task = current();
    current_task.inner() as *const _ as *mut ktask::TaskInner
}

fn registered_init_test_thread(thread: &Thread) {
    if let Some(init_thread) = *INIT_TEST_THREAD_HOOK.lock() {
        init_thread(thread);
    }
}

struct InstalledTestThread {
    previous_task_ext: Option<KTaskExt>,
    previous_page_table_root: karch::HwPageTableRoot,
}

impl InstalledTestThread {
    fn install(init_thread: InitTestThreadHook) -> KResult<Self> {
        let current_task = current();
        let tid = current_task.id().as_u64() as Pid;
        let pid = alloc_test_process_id();

        let mut aspace = memspace::MmSpace::new_user_empty()?;
        ksignal::map_signal_trampoline(&mut aspace)?;
        let aspace = Arc::new(Mutex::new(aspace));

        let proc = kprocess::Process::new_init(pid);
        proc.add_thread(tid);

        let proc_state = ProcessState::new(
            proc,
            "[unittest-user]".into(),
            Arc::new(vec![]),
            aspace,
            kfs::new_process_fs_context(),
            Arc::new(SpinNoIrq::new(SignalActions::default())),
            None,
            Credentials::root(),
            ProcessStateConfig::default(),
        );
        let thr = Thread::new(tid, proc_state);
        init_thread(&thr);

        let page_table_root = thr.proc_state.address_space().lock().page_table_hw_root();
        let task_ptr = current_task_ptr();
        let previous_page_table_root = karch::read_user_page_table();

        // SAFETY: `task_ptr` is derived from `current()` and still names the current task
        // for the duration of this installation. `NoPreempt` prevents switching away while
        // we update the task context, so the current task and its context stay stable.
        // Converting the immutable `ctx()` pointer to `*mut` is sound here because this
        // code has exclusive access to the current task context during the no-preempt
        // critical section.
        let previous_task_ext = unsafe {
            let _no_preempt = NoPreempt::new();

            if let Some(ext) = (*task_ptr).task_ext() {
                ext.on_leave();
            }

            let ctx_ptr: *mut khal::context::TaskContext = (*task_ptr).ctx() as *const _ as *mut _;
            (*ctx_ptr).set_page_table_root(page_table_root);
            karch::write_user_page_table(page_table_root);

            (*task_ptr).task_ext_mut().replace(KTaskExt::from_impl(thr))
        };

        // SAFETY: `task_ptr` still points to the current task immediately after the
        // installation above, and `on_enter` only observes the freshly installed task ext.
        unsafe {
            if let Some(ext) = (*task_ptr).task_ext() {
                ext.on_enter();
            }
        }

        Ok(Self {
            previous_task_ext,
            previous_page_table_root,
        })
    }
}

impl Drop for InstalledTestThread {
    fn drop(&mut self) {
        let task_ptr = current_task_ptr();
        // SAFETY: Drop runs on the same current task that installed the temporary user
        // thread runtime. `NoPreempt` keeps the task context stable while we restore the
        // previous page table root and task extension. The `ctx()` const-to-mut cast is
        // sound because this critical section has exclusive access to the current task
        // context.
        unsafe {
            {
                let _no_preempt = NoPreempt::new();

                if let Some(ext) = (*task_ptr).task_ext() {
                    ext.on_leave();
                }

                let ctx_ptr: *mut khal::context::TaskContext =
                    (*task_ptr).ctx() as *const _ as *mut _;
                (*ctx_ptr).set_page_table_root(self.previous_page_table_root);
                karch::write_user_page_table(self.previous_page_table_root);

                *(*task_ptr).task_ext_mut() = self.previous_task_ext.take();
            }
            if let Some(ext) = (*task_ptr).task_ext() {
                ext.on_enter();
            }
        }
    }
}

/// Run a unittest with a temporary userspace thread runtime installed.
pub fn run_with_test_user_thread(
    test: &TestDescriptor,
    init_thread: InitTestThreadHook,
) -> TestResult {
    let _installed_thread = match InstalledTestThread::install(init_thread) {
        Ok(guard) => guard,
        Err(error) => {
            error!(
                "failed to install unittest user-thread runtime for {}:{}: {error:?}",
                test.module, test.name
            );
            return TestResult::Failed;
        }
    };

    (test.test_fn)()
}

const USER_TEST_STACK_SIZE: usize = 64 * 1024;

/// Run a unittest on a temporary userspace stack inside a temporary userspace thread runtime.
pub fn run_with_test_user_stack(
    test: &TestDescriptor,
    init_thread: InitTestThreadHook,
) -> TestResult {
    let _installed_thread = match InstalledTestThread::install(init_thread) {
        Ok(guard) => guard,
        Err(error) => {
            error!(
                "failed to install unittest user-thread runtime for {}:{}: {error:?}",
                test.module, test.name
            );
            return TestResult::Failed;
        }
    };

    let user_stack = match TestUserBuffer::new(USER_TEST_STACK_SIZE) {
        Ok(stack) => stack,
        Err(error) => {
            error!(
                "failed to allocate user test stack for {}:{}: {error:?}",
                test.module, test.name
            );
            return TestResult::Failed;
        }
    };

    let stack_top = user_stack.as_user_ptr::<u8>() as usize + user_stack.len();

    let _no_preempt = NoPreempt::new();
    // SAFETY: `TestUserBuffer` owns a valid user-mapped stack region of `USER_TEST_STACK_SIZE`
    // bytes, and `stack_top` is the exclusive top-of-stack pointer for that region. Keeping
    // preemption disabled avoids switching away while the test trampoline enters user mode.
    unsafe { unittest::run_test_on_user_stack(test, stack_top) }
}

/// Register the shared unittest runtime using a crate-provided test-thread initialization hook.
pub fn register_unittest_runtime(init_thread: InitTestThreadHook) {
    *INIT_TEST_THREAD_HOOK.lock() = Some(init_thread);
    unittest::register_custom_test_executor(run_registered_test_user_thread);
    unittest::register_user_test_executor(run_registered_test_user_stack);
}

fn run_registered_test_user_thread(test: &TestDescriptor) -> TestResult {
    run_with_test_user_thread(test, registered_init_test_thread)
}

fn run_registered_test_user_stack(test: &TestDescriptor) -> TestResult {
    run_with_test_user_stack(test, registered_init_test_thread)
}

pub struct TestUserBuffer {
    aspace: Arc<Mutex<memspace::MmSpace>>,
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
