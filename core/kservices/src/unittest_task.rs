// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Unittest-only helpers for installing a simulated user thread runtime.

#![cfg(unittest)]

use alloc::{string::ToString, sync::Arc, vec};
use core::sync::atomic::{AtomicU32, Ordering};

use kcred::Credentials;
use kerrno::KResult;
use kprocess::Pid;
use ksignal::api::SignalActions;
use ksync::{
    Mutex,
    spin::{NoPreempt, SpinNoIrq},
};
use ktask::{KTaskExt, TaskExt, current};
use kthread::{ProcessState, ProcessStateConfig, Thread};
use unittest::{TestDescriptor, TestResult};
use unittest_support::TestUserBuffer;

static NEXT_TEST_PROCESS_ID: AtomicU32 = AtomicU32::new(0x7000_0000);
fn alloc_test_process_id() -> Pid {
    NEXT_TEST_PROCESS_ID.fetch_add(1, Ordering::Relaxed)
}

fn current_task_ptr() -> *mut ktask::TaskInner {
    let current_task = current();
    current_task.inner() as *const _ as *mut ktask::TaskInner
}

struct InstalledTestThread {
    previous_task_ext: Option<KTaskExt>,
    previous_page_table_root: karch::HwPageTableRoot,
}

impl InstalledTestThread {
    fn install<F>(init_thread: F) -> KResult<Self>
    where
        F: FnOnce(&Thread),
    {
        let current_task = current();
        let tid = current_task.id().as_u64() as Pid;
        let pid = alloc_test_process_id();

        // `new_user_empty` installs the standard user range and any required
        // kernel mappings for the target architecture.
        let mut aspace = memspace::AddrSpace::new_user_empty()?;
        ksignal::map_signal_trampoline(&mut aspace)?;
        let aspace = Arc::new(Mutex::new(aspace));

        let proc = kprocess::Process::new_init(pid);
        proc.add_thread(tid);

        let proc_state = ProcessState::new(
            proc,
            "[unittest-user]".to_string(),
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

        let page_table_root = thr.proc_state.address_space().lock().page_table_root();
        let task_ptr = current_task_ptr();
        let previous_page_table_root = karch::read_user_page_table();

        let previous_task_ext = unsafe {
            let _no_preempt = NoPreempt::new();

            if let Some(ext) = (*task_ptr).task_ext() {
                ext.on_leave();
            }

            let ctx_ptr: *mut khal::context::TaskContext = (*task_ptr).ctx() as *const _ as *mut _;
            (*ctx_ptr).set_page_table_root(page_table_root.into());
            karch::write_user_page_table(page_table_root.into());
            karch::flush_tlb(None);

            (*task_ptr).task_ext_mut().replace(KTaskExt::from_impl(thr))
        };

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
                karch::flush_tlb(None);

                *(*task_ptr).task_ext_mut() = self.previous_task_ext.take();
            }
            if let Some(ext) = (*task_ptr).task_ext() {
                ext.on_enter();
            }
        }
    }
}

pub fn run_with_test_user_thread<F>(test: &TestDescriptor, init_thread: F) -> TestResult
where
    F: FnOnce(&Thread),
{
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

pub fn run_with_test_user_stack<F>(test: &TestDescriptor, init_thread: F) -> TestResult
where
    F: FnOnce(&Thread),
{
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

    // Keep preemption disabled while running on a temporary userspace stack.
    let _no_preempt = NoPreempt::new();
    unsafe { unittest::run_test_on_user_stack(test, stack_top) }
}

#[cfg(feature = "tee")]
fn init_unittest_thread(thread: &Thread) {
    crate::tee::set_tee_session_ctx(thread);
}

#[cfg(not(feature = "tee"))]
fn init_unittest_thread(_: &Thread) {}

pub fn register_unittest_runtime() {
    unittest::register_custom_test_executor(|test| {
        run_with_test_user_thread(test, init_unittest_thread)
    });
    unittest::register_user_test_executor(|test| {
        run_with_test_user_stack(test, init_unittest_thread)
    });
}
