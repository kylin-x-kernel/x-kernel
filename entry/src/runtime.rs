// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Entry-side runtime bootstrap and unittest glue.

use kvfs::{ST_NODEV, ST_NOEXEC, ST_NOSUID, ST_RELATIME};

/// Initialize VFS mounts and the alarm task.
pub fn init_runtime() {
    info!("Initialize VFS...");
    devfs::capture_firmware_dtb_snapshot();
    let mounts = kfs::VirtualFsMounts {
        devfs: devfs::new_devfs(),
        dev_shm: memfs::MemoryFs::new_with_name_and_flags(
            "tmpfs",
            ST_NOSUID | ST_NODEV | ST_RELATIME,
        ),
        tmpfs: memfs::MemoryFs::new_with_name_and_flags(
            "tmpfs",
            ST_NOSUID | ST_NODEV | ST_RELATIME,
        ),
        procfs: procfs::new_procfs(),
        sysfs: memfs::MemoryFs::new_with_name_and_flags(
            "sysfs",
            ST_NOSUID | ST_NODEV | ST_NOEXEC | ST_RELATIME,
        ),
    };
    kfs::mount_virtual_filesystems(mounts).expect("Failed to mount vfs");

    if let Err(err) = devfs::bind_dev_log() {
        if err != kerrno::LinuxError::ENOSYS && err != kerrno::LinuxError::EOPNOTSUPP {
            panic!("Failed to bind dev-log: {err}");
        }
        warn!("/dev/log not available: {err}");
    }

    info!("Initialize alarm...");
    kthread::spawn_alarm_task();
}

#[cfg(feature = "unittest")]
mod unittest_runtime {
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

                let ctx_ptr: *mut khal::context::TaskContext =
                    (*task_ptr).ctx() as *const _ as *mut _;
                (*ctx_ptr).set_page_table_root(page_table_root.into());
                karch::write_user_page_table(page_table_root.into());
                karch::flush_tlb(None);

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

        let _no_preempt = NoPreempt::new();
        // SAFETY: `TestUserBuffer` owns a valid user-mapped stack region of `USER_TEST_STACK_SIZE`
        // bytes, and `stack_top` is the exclusive top-of-stack pointer for that region. Keeping
        // preemption disabled avoids switching away while the test trampoline enters user mode.
        unsafe { unittest::run_test_on_user_stack(test, stack_top) }
    }

    #[cfg(any(target_arch = "aarch64", target_arch = "x86_64"))]
    fn init_unittest_tee_context(thread: &Thread) {
        if kbuild_config::KFEAT_TEE {
            tee_kernel::tee::set_tee_session_ctx(thread);
        }
    }

    #[cfg(not(any(target_arch = "aarch64", target_arch = "x86_64")))]
    fn init_unittest_tee_context(_: &Thread) {}

    pub fn register_unittest_runtime() {
        unittest::register_custom_test_executor(|test| {
            run_with_test_user_thread(test, init_unittest_tee_context)
        });
        unittest::register_user_test_executor(|test| {
            run_with_test_user_stack(test, init_unittest_tee_context)
        });
    }
}

#[cfg(feature = "unittest")]
pub use unittest_runtime::{register_unittest_runtime, run_with_test_user_thread};
