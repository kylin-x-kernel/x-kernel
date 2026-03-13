// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Unittest-only helpers for installing a simulated user thread runtime.

#![cfg(unittest)]

use alloc::{string::ToString, sync::Arc, vec};
use core::sync::atomic::{AtomicU32, Ordering};

use kcore::task::{ProcessData, Thread};
use kerrno::KResult;
use kprocess::Pid;
use ksignal::api::SignalActions;
use ksync::{
    Mutex,
    spin::{NoPreempt, SpinNoIrq},
};
use ktask::{KTaskExt, TaskExt, current};
use memaddr::PhysAddr;
use unittest::{TestDescriptor, TestResult};

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
    previous_page_table_root: PhysAddr,
}

impl InstalledTestThread {
    fn install<F>(init_thread: F) -> KResult<Self>
    where
        F: FnOnce(&Thread),
    {
        let current_task = current();
        let tid = current_task.id().as_u64() as Pid;
        let pid = alloc_test_process_id();

        let mut aspace = kcore::mm::new_user_aspace_empty()?;
        kcore::mm::copy_from_kernel(&mut aspace)?;
        kcore::mm::map_trampoline(&mut aspace)?;
        let aspace = Arc::new(Mutex::new(aspace));

        let proc = kprocess::Process::new_init(pid);
        proc.add_thread(tid);

        let proc_data = ProcessData::new(
            proc,
            "[unittest-user]".to_string(),
            Arc::new(vec![]),
            aspace,
            Arc::new(SpinNoIrq::new(SignalActions::default())),
            None,
        );
        let thr = Thread::new(tid, proc_data);
        init_thread(&thr);

        let page_table_root = thr.proc_data.aspace.lock().page_table_root();
        let task_ptr = current_task_ptr();
        let previous_page_table_root = karch::read_user_page_table();

        let previous_task_ext = unsafe {
            let _no_preempt = NoPreempt::new();

            if let Some(ext) = (*task_ptr).task_ext() {
                ext.on_leave();
            }

            let ctx_ptr: *mut khal::context::TaskContext = (*task_ptr).ctx() as *const _ as *mut _;
            (*ctx_ptr).set_page_table_root(page_table_root);
            karch::write_user_page_table(page_table_root);
            karch::flush_tlb(None);

            let previous_task_ext =
                core::mem::replace((*task_ptr).task_ext_mut(), Some(KTaskExt::from_impl(thr)));

            previous_task_ext
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
}
