// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Real fork/exit/wait controls for fresh-MM exec activation.
//!
//! The synthetic ELF supports installation and populated-stack inspection only:
//! SimpleFile has no folio reader and its bytes are not a runnable program.
//! These tests do not enter userspace; exit_tail covers last-user MM retirement.

use alloc::{format, sync::Arc, vec, vec::Vec};
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use kerrno::{KError, KResult, LinuxError};
use khal::{paging::MappingFlags, uspace::UserContext};
use kprocess::{
    AsThread, ForkAddressSpace, ForkFdTable, ForkFs, ForkParent, ForkSignalActions,
    ProcessForkConfig,
};
use ksignal::{SignalAction, SignalDisposition, SignalInfo, SignalSet, SignalStack, Signo};
use ktask::{TaskInner, WaitQueue};
use ktime_types::TimeSpan;
use kvfs::{
    DirMapping, FileSystemType, Mount, NodeFlags, NodePermission, Path, SimpleDir, SimpleFile,
    SimpleFs, VfsInode,
};
use osvm::{VirtMutPtr, VirtPtr};
use unittest::{TestResult, def_test};
use unittest_support::TestUserValue;

use crate::task::exit::exit_tail::{bounded_wait, capture_root, check_current_root};

/// Request-local observation/injection after activation and registration reset.
pub(in crate::task) struct ExecProbe {
    /// Number of times this request activated its fresh address space.
    pub(super) after_exchange: usize,
    case: Case,
    partial_hits: Arc<AtomicUsize>,
    update_hits: Arc<AtomicUsize>,
    empty_migration_hits: usize,
    old_mm_id: u64,
    sentinel_addr: usize,
    switch: Arc<PeerSwitch>,
    retirement: Option<Arc<Retirement>>,
    #[cfg(feature = "tee")]
    tee_session: Option<tee_kernel::tee::ExecSessionFixture>,
}

struct Retirement {
    pin: memspace::process_lifetime::MmPin,
    is_ready: AtomicBool,
    can_exec: AtomicBool,
}

fn assert_retirement_pending(mut pending: SignalSet) {
    assert!(pending.remove(Signo::SIGUSR1));
    assert!(pending.remove(Signo::SIGTERM));
    assert!(pending.is_empty());
}

impl Retirement {
    #[expect(
        clippy::panic,
        reason = "CG-021-no-panic-on-reachable-failure: test scenario: a retired MM gaining a \
                  user is the regression this exec lifecycle check reports, after dropping that \
                  user outside the MM lock"
    )]
    fn block_release_and_queue_signals(&self, child: &ktask::KtaskRef, sentinel: usize) {
        bounded_wait(|| self.is_ready.load(Ordering::Acquire));
        let thread = child.as_thread();
        let mm = self.pin.address_space().lock();
        assert!(mm.find_vma(sentinel.into()).is_some());
        // READY publishes all old-MM setup; GO is written only while M is held.
        self.can_exec.store(true, Ordering::Release);
        bounded_wait(|| {
            // Never lock the observed binding: before exchange it is our M.
            let has_new_binding = {
                let bound = thread
                    .process()
                    .pinned_address_space_for_teardown_observation()
                    .unwrap();
                !Arc::ptr_eq(&bound, self.pin.address_space())
            };
            has_new_binding && child.state() == ktask::TaskState::Blocked
        });
        assert!(!self.pin.is_user_alive());
        let unexpected_user = self.pin.try_upgrade_user();
        if unexpected_user.is_some() {
            // Even a regression must not drop a potential last user under M.
            drop(mm);
            drop(unexpected_user);
            panic!("retired MM accepted a new user");
        }
        assert_eq!(thread.clear_child_tid(), 0x4000);
        assert_eq!(thread.robust_list_head(), 0x5000);
        let signal = thread.signal_manager();
        let stack = signal.stack();
        assert_eq!((stack.sp, stack.flags, stack.size), (0x6000, 0, 0x2000));
        assert!(mm.find_vma(sentinel.into()).is_some());
        assert!(signal.pending().is_empty());
        // Enqueue/interrupt cannot acquire M or wait for this child to respond.
        assert!(signal.send_signal(SignalInfo::new_kernel(Signo::SIGUSR1)));
        assert!(signal.send_signal(SignalInfo::new_kernel(Signo::SIGTERM)));
        assert_retirement_pending(signal.pending());
        child.interrupt();
        // No child acknowledgement is required while we prevent final clear.
        drop(mm);
    }
}

struct PeerSwitch {
    stage: Arc<AtomicUsize>,
    empty_acknowledgement: WaitQueue,
    cpu: usize,
    target_cpu: Option<usize>,
}

const EMPTY_READY: usize = 1;
const EMPTY_ACKNOWLEDGED: usize = 2;
const PARTIAL_READY: usize = 3;
const PARTIAL_ACKNOWLEDGED: usize = 4;
// Published after peer acknowledgement and destination-CPU checks.
const PARTIAL_MIGRATED: usize = 5;

struct RetainedImage {
    image: kprocess::LiveAddressSpace,
    pin: memspace::process_lifetime::MmPin,
    mm_id: u64,
    root: usize,
    heap: usize,
    key: kfutex::FutexKey,
    sentinel: usize,
}

impl RetainedImage {
    fn capture(process: &kprocess::Process, inherited_sentinel: usize) -> Self {
        let first_heap = kaddr_layout::USER_HEAP_BASE + 4096;
        process.set_heap_top(first_heap).unwrap();
        let image = process.address_space().unwrap();
        assert_eq!(image.heap_top(), first_heap);
        let heap = first_heap + 4096;
        process.set_heap_top(heap).unwrap();
        assert_eq!(image.heap_top(), heap);
        // TestUserValue uses the heap range also mapped by the new image.
        // A private page below it supplies an unambiguous old-only VA.
        let sentinel = kaddr_layout::USER_HEAP_BASE - 4096;
        let (mm_id, root) = {
            let mut mm = image.lock();
            assert!(mm.find_vma(inherited_sentinel.into()).is_some());
            assert!(mm.find_vma(sentinel.into()).is_none());
            mm.map(
                sentinel.into(),
                4096,
                MappingFlags::USER | MappingFlags::READ | MappingFlags::WRITE,
                false,
                memspace::VmRuntimeRef::new_anon_private(
                    sentinel.into(),
                    khal::paging::PageSize::Size4K,
                ),
            )
            .unwrap();
            assert!(mm.find_vma(sentinel.into()).is_some());
            (mm.mm_id(), mm.page_table_root().as_usize())
        };
        let mm = process
            .pinned_address_space_for_teardown_observation()
            .unwrap();
        let user = memspace::MmSpace::acquire_user(mm).unwrap();
        let pin = user.pin();
        // Only image remains as the extra active user across exec; pin is structural.
        drop(user);
        let key = crate::sync::observe_private_futex_key(sentinel).unwrap();
        assert_eq!(
            key,
            kfutex::FutexKey::resolve_private(mm_id, sentinel).unwrap()
        );
        assert_eq!(mm_id, kprocess::current_user_mm_id());
        assert_eq!(check_current_root(None), root);
        Self {
            image,
            pin,
            mm_id,
            root,
            heap,
            key,
            sentinel,
        }
    }

    fn verify_and_release(self, process: &kprocess::Process, entry: usize) {
        let Self {
            image,
            pin,
            mm_id,
            root,
            heap,
            key,
            sentinel,
        } = self;
        let current = process.address_space().unwrap();
        let current_id = process.mm_id().unwrap();
        assert_ne!(current_id, mm_id);
        assert_eq!(current_id, kprocess::current_user_mm_id());
        assert_eq!(current.heap_top(), kaddr_layout::USER_HEAP_BASE);
        let new_heap = heap + 4096;
        process.set_heap_top(new_heap).unwrap();
        assert_eq!(current.heap_top(), new_heap);
        assert_eq!(image.heap_top(), heap);
        {
            let old = image.lock();
            assert_eq!(old.mm_id(), mm_id);
            assert_eq!(old.page_table_root().as_usize(), root);
            assert!(old.find_vma(sentinel.into()).is_some());
        }
        let new_root = {
            let new = current.lock();
            assert_eq!(new.mm_id(), current_id);
            assert!(new.find_vma(sentinel.into()).is_none());
            assert!(new.find_vma(entry.into()).is_some());
            new.page_table_root().as_usize()
        };
        assert_ne!(new_root, root);
        assert_eq!(check_current_root(None), new_root);
        // PRIVATE keys need a valid aligned VA, not a mapping in the new image.
        let new_key = crate::sync::observe_private_futex_key(sentinel).unwrap();
        assert_ne!(new_key, key);
        assert_eq!(
            new_key,
            kfutex::FutexKey::resolve_private(current_id, sentinel).unwrap()
        );
        assert_eq!(
            kfutex::FutexKey::resolve_private(image.lock().mm_id(), sentinel).unwrap(),
            key
        );
        assert!(pin.is_user_alive());
        let observed = Arc::downgrade(pin.address_space());
        // We run on N with no guard held: this is M's actual last active user.
        assert!(ktask::is_sleepable_context());
        drop(image);
        assert!(!pin.is_user_alive());
        assert!(pin.try_upgrade_user().is_none());
        assert_eq!(pin.address_space().lock().vmas().count(), 0);
        assert_eq!(current.heap_top(), new_heap);
        {
            let new = current.lock();
            assert_eq!(new.mm_id(), current_id);
            assert!(new.find_vma(entry.into()).is_some());
        }
        assert_eq!(check_current_root(None), new_root);
        // Do not mask N's later exit teardown with our observation capability.
        drop(current);
        drop(pin);
        bounded_wait(|| observed.upgrade().is_none());
    }
}

impl ExecProbe {
    pub(super) fn should_fail_precheck(&self) -> bool {
        self.case == Case::Precheck
    }

    fn should_fail_after_exchange(&self) -> bool {
        matches!(self.case, Case::Fatal | Case::Retirement)
    }

    pub(super) fn attach_failure_probes(
        &mut self,
        image: kexec::PreparedExecImage,
        update: kprocess::ProcessExecUpdate,
    ) -> (kexec::PreparedExecImage, kprocess::ProcessExecUpdate) {
        let image = if self.case == Case::Partial {
            image.with_partial_install_probe(kexec::PartialInstallProbe {
                hits: self.partial_hits.clone(),
                stage: self.switch.stage.clone(),
                target_cpu: self.switch.target_cpu,
                observe: observe_partial,
            })
        } else {
            image
        };
        let update = if self.case == Case::Update {
            update.with_failure_probe(self.update_hits.clone())
        } else {
            update
        };
        (image, update)
    }

    pub(super) fn observe_update(&self, process: &kprocess::Process, entry: usize, sp: usize) {
        #[cfg(feature = "tee")]
        if let Some(session) = &self.tee_session {
            session.assert_live(&kprocess::current_user_thread());
        }
        if self.case != Case::Update {
            return;
        }
        assert_eq!(self.update_hits.load(Ordering::Relaxed), 1);
        assert_eq!(process.exe_path().unwrap(), "/app");
        assert_eq!(process.cmdline().unwrap().as_slice(), &["app"]);
        assert_eq!(process.heap_top().unwrap(), kaddr_layout::USER_HEAP_BASE);
        assert!(matches!(
            process
                .signal_manager()
                .unwrap()
                .signal_action(Signo::SIGUSR1)
                .unwrap()
                .disposition,
            SignalDisposition::Default
        ));
        {
            let mm = process.address_space().unwrap();
            let mm = mm.lock();
            assert!(
                mm.find_vma(entry.into())
                    .unwrap()
                    .flags()
                    .contains(MappingFlags::USER | MappingFlags::EXECUTE)
            );
            assert!(mm.find_vma(sp.into()).is_some());
        }
        assert_eq!((sp as *const usize).read_vm().unwrap(), 1);
        check_current_root(None);
    }

    /// Checks activation before any image mapping; all MM owners drop before error.
    pub(super) fn on_activated(&mut self) -> KResult<()> {
        self.after_exchange += 1;
        let root = check_current_root(Some(self.sentinel_addr));
        let thread = kprocess::current_user_thread();
        assert_ne!(thread.process().mm_id().unwrap(), self.old_mm_id);
        assert_eq!(
            thread.process().heap_top().unwrap(),
            kaddr_layout::USER_HEAP_BASE
        );
        assert_eq!(thread.clear_child_tid(), 0);
        assert_eq!(thread.robust_list_head(), 0);
        let stack = thread.signal_manager().stack();
        let empty = SignalStack::default();
        assert_eq!(
            (stack.sp, stack.flags, stack.size),
            (empty.sp, empty.flags, empty.size)
        );
        let switch = &self.switch;
        #[cfg(feature = "tee")]
        if let Some(session) = &self.tee_session {
            session.assert_live(&thread);
        }
        assert_eq!(khal::percpu::this_cpu_id().as_usize(), switch.cpu);
        if let Some(retirement) = &self.retirement {
            assert_retirement_pending(thread.signal_manager().pending());
            assert!(!retirement.pin.is_user_alive());
            assert!(retirement.pin.try_upgrade_user().is_none());
            assert_eq!(retirement.pin.address_space().lock().vmas().count(), 0);
            assert_eq!(check_current_root(Some(self.sentinel_addr)), root);
            assert!(self.should_fail_after_exchange());
            return Err(KError::NoMemory);
        }
        // Release/acquire orders the peer's user-memory check before resumption.
        switch.stage.store(EMPTY_READY, Ordering::Release);
        // The parent must observe a real blocked interval. Periodic polling
        // makes that interval transient and can stay in phase with its poll.
        assert!(
            !switch
                .empty_acknowledgement
                .wait_timeout_until(TimeSpan::from_secs(5), || {
                    switch.stage.load(Ordering::Acquire) == EMPTY_ACKNOWLEDGED
                }),
            "exec activation peer acknowledgement timeout"
        );
        assert_eq!(khal::percpu::this_cpu_id().as_usize(), switch.cpu);
        assert_eq!(check_current_root(Some(self.sentinel_addr)), root);
        if self.should_fail_after_exchange() {
            if let Some(target) = switch.target_cpu {
                let migrate = {
                    let image = thread.process().address_space().unwrap();
                    let mm = image.lock();
                    assert_eq!(mm.vmas().count(), 0);
                    new_image_migration(&mm, target)
                };
                migrate();
                assert_eq!(check_current_root(Some(self.sentinel_addr)), root);
                self.empty_migration_hits += 1;
                unittest::ktest_println!(
                    "exec empty migration verified: CPU {} -> {}",
                    switch.cpu,
                    khal::percpu::this_cpu_id().as_usize()
                );
            }
            Err(KError::NoMemory)
        } else {
            Ok(())
        }
    }
}

#[derive(Clone, Copy, PartialEq)]
enum Case {
    Malformed,
    Precheck,
    Partial,
    Update,
    Unrelated,
    Success,
    RetainedSuccess,
    Fatal,
    Retirement,
}

impl Case {
    fn is_early(self) -> bool {
        matches!(self, Self::Malformed | Self::Precheck)
    }

    fn is_fatal(self) -> bool {
        matches!(
            self,
            Self::Fatal | Self::Partial | Self::Update | Self::Retirement
        )
    }

    fn is_success(self) -> bool {
        matches!(
            self,
            Self::Success | Self::RetainedSuccess | Self::Unrelated
        )
    }

    fn is_private(self) -> bool {
        matches!(self, Self::RetainedSuccess | Self::Retirement)
    }
}

fn new_image_migration(mm: &memspace::MmSpace, target: usize) -> impl FnOnce() + use<> {
    let check = capture_root(mm);
    let residency = mm.cpu_residency().clone();
    // Only cached metadata escapes; empty-site guards drop before this executes.
    move || {
        check();
        assert_ne!(khal::percpu::this_cpu_id().as_usize(), target);
        assert!(!residency.snapshot().get(target));
        assert!(karch::local_irq_enabled());
        assert!(ktask::is_sleepable_context());
        assert!(ktask::set_current_affinity(ktask::KCpuMask::one_shot(
            target
        )));
        assert_eq!(khal::percpu::this_cpu_id().as_usize(), target);
        check();
        assert!(residency.snapshot().get(target));
    }
}

fn observe_partial(mm: &memspace::MmSpace, stage: &AtomicUsize, target_cpu: Option<usize>) {
    let cpu = khal::percpu::this_cpu_id().as_usize();
    let check = capture_root(mm);
    check();
    // The peer touches only its old MM, never this borrowed new-MM mutex.
    // Release/acquire plus the peer's Blocked check proves an actual switch.
    assert!(karch::local_irq_enabled());
    stage.store(PARTIAL_READY, Ordering::Release);
    bounded_wait(|| stage.load(Ordering::Acquire) == PARTIAL_ACKNOWLEDGED);
    assert_eq!(khal::percpu::this_cpu_id().as_usize(), cpu);
    check();
    if let Some(target) = target_cpu {
        let mappings = mm.vmas().count();
        // Borrowed N stays locked by this task; scheduler and old peer do not need it.
        new_image_migration(mm, target)();
        assert_eq!(mm.vmas().count(), mappings);
        assert_eq!(
            stage.compare_exchange(
                PARTIAL_ACKNOWLEDGED,
                PARTIAL_MIGRATED,
                Ordering::AcqRel,
                Ordering::Acquire,
            ),
            Ok(PARTIAL_ACKNOWLEDGED)
        );
        unittest::ktest_println!(
            "exec partial migration verified: CPU {} -> {}",
            cpu,
            khal::percpu::this_cpu_id().as_usize()
        );
    }
}

pub(in crate::task) fn executable_root(data: Vec<u8>) -> Path {
    static TEST_FS: FileSystemType = FileSystemType::internal("exec-lifecycle-test");
    let size = data.len() as u64;
    let super_block = SimpleFs::new_with(&TEST_FS, 0, |fs| {
        let file = SimpleFile::new_regular_with_permission(
            fs.clone(),
            NodePermission::from_bits_truncate(0o555),
            move || Ok(data.clone()),
        );
        // Lookup must retain the size needed by ELF preparation's scalar reads.
        let init = file.inode_init().with_size(size);
        let inode = VfsInode::new_file_with_operations(
            file.clone(),
            kvfs::empty_inode_operations(),
            file,
            NodeFlags::empty(),
            init,
        );
        let mut entries = DirMapping::new();
        entries.add_child("app", move |lookup, name| {
            Ok(lookup.file_from_inode(name, inode.clone()))
        });
        SimpleDir::new_maker(fs, Arc::new(entries))
    });
    Mount::new_root(&super_block).root_path()
}

#[expect(
    clippy::too_many_lines,
    reason = "CG-028-focused-functions: unittest scenario: one linear exec child script in a file \
              test module"
)]
fn run_child(
    case: Case,
    sentinel_addr: usize,
    has_live_success: &AtomicBool,
    switch: Arc<PeerSwitch>,
    retirement: Option<Arc<Retirement>>,
    #[cfg(feature = "tee")] has_tee_session: bool,
) {
    let thread = kprocess::current_user_thread();
    let process = thread.process();
    let mm_id = process.mm_id().unwrap();
    let old_root = check_current_root(None);
    let old_heap = process.heap_top().unwrap();
    let mut uctx = UserContext::new(0x1234, 0x8000usize.into(), 0x5678);
    uctx.set_tls(0x9000);
    uctx.set_retval(0xfeed);
    let context_before = format!("{uctx:?}");
    let original_tid = thread.clear_child_tid();
    let original_robust = thread.robust_list_head();
    let signal = thread.signal_manager();
    let original_stack = signal.stack();
    let mut probe = ExecProbe {
        after_exchange: 0,
        case,
        partial_hits: Arc::new(AtomicUsize::new(0)),
        update_hits: Arc::new(AtomicUsize::new(0)),
        empty_migration_hits: 0,
        old_mm_id: mm_id,
        sentinel_addr,
        switch,
        retirement,
        #[cfg(feature = "tee")]
        tee_session: has_tee_session.then(tee_kernel::tee::ExecSessionFixture::provision),
    };
    if case.is_early() {
        assert_eq!((sentinel_addr as *const u64).read_vm().unwrap(), 0x1234);
        (sentinel_addr as *mut u64).write_vm(0x5678).unwrap();
    }
    // Early errors preserve these; activated targets must reset before fatal cleanup.
    thread.set_clear_child_tid(0x4000);
    thread.set_robust_list_head(0x5000);
    signal.set_stack(SignalStack {
        sp: 0x6000,
        flags: 0,
        size: 0x2000,
    });
    if case == Case::Update {
        process
            .set_heap_top(kaddr_layout::USER_HEAP_BASE + 4096)
            .unwrap();
        process
            .signal_manager()
            .unwrap()
            .set_signal_action(
                Signo::SIGUSR1,
                SignalAction {
                    disposition: SignalDisposition::Handler(unused_handler),
                    ..SignalAction::default()
                },
            )
            .unwrap();
    }
    if let Some(retirement) = &probe.retirement {
        let signals = process.signal_manager().unwrap();
        signals
            .set_signal_action(
                Signo::SIGUSR1,
                SignalAction {
                    disposition: SignalDisposition::Handler(unused_handler),
                    ..SignalAction::default()
                },
            )
            .unwrap();
        signals
            .set_signal_action(Signo::SIGTERM, SignalAction::default())
            .unwrap();
        signal.set_blocked(SignalSet::default());
        assert!(signal.pending().is_empty());
        retirement.is_ready.store(true, Ordering::Release);
        bounded_wait(|| retirement.can_exec.load(Ordering::Acquire));
    }
    let retained =
        (case == Case::RetainedSuccess).then(|| RetainedImage::capture(process, sentinel_addr));
    let result = super::exec_owned(
        &mut uctx,
        "/app".into(),
        vec!["app".into()],
        vec![],
        if case == Case::Unrelated {
            None
        } else {
            Some(&mut probe)
        },
    );
    #[cfg(feature = "tee")]
    if let Some(session) = probe.tee_session.take() {
        // Check before ordinary success teardown; fatal exec already called do_exit.
        session.finish(
            &thread,
            case.is_fatal(),
            if case == Case::Fatal { 2 } else { 3 },
        );
    }
    assert_eq!(
        probe.partial_hits.load(Ordering::Relaxed),
        usize::from(case == Case::Partial)
    );
    assert_eq!(
        probe.update_hits.load(Ordering::Relaxed),
        usize::from(case == Case::Update)
    );
    match case {
        Case::Malformed | Case::Precheck => {
            assert_eq!(
                result,
                Err(if case == Case::Malformed {
                    KError::InvalidExecutable
                } else {
                    KError::WouldBlock
                })
            );
            assert_eq!(probe.after_exchange, 0);
            assert_eq!(process.mm_id().unwrap(), mm_id);
            assert_eq!(check_current_root(None), old_root);
            assert_eq!(process.heap_top().unwrap(), old_heap);
            assert_eq!((sentinel_addr as *const u64).read_vm().unwrap(), 0x5678);
            assert_eq!(thread.clear_child_tid(), 0x4000);
            assert_eq!(thread.robust_list_head(), 0x5000);
            let stack = signal.stack();
            assert_eq!((stack.sp, stack.flags, stack.size), (0x6000, 0, 0x2000));
            assert_eq!(format!("{uctx:?}"), context_before);
            assert!(!thread.is_exiting());
            thread.set_clear_child_tid(original_tid);
            thread.set_robust_list_head(original_robust);
            signal.set_stack(original_stack);
        }
        Case::Success | Case::RetainedSuccess | Case::Unrelated => {
            assert_eq!(result, Ok(0));
            assert_eq!(probe.after_exchange, usize::from(case != Case::Unrelated));
            assert!(!thread.is_exiting());
            assert_ne!(uctx.ip(), 0x1234);
            assert_ne!(uctx.sp(), 0x8000);
            let mut expected = UserContext::new(0, 0usize.into(), 0);
            expected.reset_for_exec();
            expected.set_ip(uctx.ip());
            expected.set_sp(uctx.sp());
            expected.set_tls(0);
            assert_eq!(format!("{uctx:?}"), format!("{expected:?}"));
            {
                let aspace = process.address_space().unwrap();
                let aspace = aspace.lock();
                assert!(
                    aspace
                        .find_vma(uctx.ip().into())
                        .unwrap()
                        .flags()
                        .contains(MappingFlags::USER | MappingFlags::EXECUTE)
                );
            }
            // The stack is populated; reading argc does not fault an ELF segment.
            assert_eq!((uctx.sp() as *const usize).read_vm().unwrap(), 1);
            if let Some(retained) = retained {
                retained.verify_and_release(process, uctx.ip());
            }
            has_live_success.store(true, Ordering::Relaxed);
        }
        Case::Fatal | Case::Partial | Case::Update | Case::Retirement => {
            assert_eq!(result, Ok(0));
            assert_eq!(probe.after_exchange, 1);
            assert!(thread.is_exiting());
            assert_eq!(
                signal.process().signal_action(Signo::SIGUSR1).err(),
                Some(KError::NoSuchProcess)
            );
            if let Some(target) = probe.switch.target_cpu {
                assert_eq!(khal::percpu::this_cpu_id().as_usize(), target);
                assert_eq!(probe.empty_migration_hits, usize::from(case == Case::Fatal));
                if case == Case::Partial {
                    assert_eq!(probe.switch.stage.load(Ordering::Acquire), PARTIAL_MIGRATED);
                }
            } else {
                assert_eq!(probe.empty_migration_hits, 0);
            }
            assert!(!has_live_success.load(Ordering::Relaxed));
            assert_eq!(format!("{uctx:?}"), context_before);
            let pending = signal.pending();
            if case == Case::Retirement {
                assert_retirement_pending(pending);
            }
            assert!(posix_process::check_signals(&thread, &mut uctx, None).is_none());
            let after = signal.pending();
            assert!((pending & !after).is_empty() && (after & !pending).is_empty());
            assert_eq!(format!("{uctx:?}"), context_before);
            // do_exit returns to kernel continuations. Never call it a second time.
            return;
        }
    }
    // Pending handlers and fatal signals must not run on the exit continuation.
    extern "C" fn unused_handler(_: i32) {}
    let process_signals = process.signal_manager().unwrap();
    process_signals
        .set_signal_action(
            Signo::SIGUSR1,
            SignalAction {
                disposition: SignalDisposition::Handler(unused_handler),
                ..SignalAction::default()
            },
        )
        .unwrap();
    process_signals
        .set_signal_action(Signo::SIGTERM, SignalAction::default())
        .unwrap();
    signal.set_blocked(SignalSet::default());
    assert!(signal.send_signal(SignalInfo::new_kernel(Signo::SIGUSR1)));
    assert!(signal.send_signal(SignalInfo::new_kernel(Signo::SIGTERM)));
    let pending = signal.pending();
    let context = format!("{uctx:?}");
    posix_process::do_exit(0, false);
    assert!(thread.is_exiting());
    assert_eq!(
        process_signals.signal_action(Signo::SIGUSR1).err(),
        Some(KError::NoSuchProcess)
    );
    assert!(posix_process::check_signals(&thread, &mut uctx, None).is_none());
    let after = signal.pending();
    assert!((pending & !after).is_empty() && (after & !pending).is_empty());
    assert_eq!(format!("{uctx:?}"), context);
}

/// Requires the unittest executor's real current-user parent.
fn run_case(case: Case) {
    run_case_with_session(
        case,
        None,
        #[cfg(feature = "tee")]
        false,
    );
}

#[expect(
    clippy::too_many_lines,
    reason = "CG-028-focused-functions: unittest scenario: one linear exec session script in a \
              file test module"
)]
fn run_case_with_session(
    case: Case,
    target_cpu: Option<usize>,
    #[cfg(feature = "tee")] has_tee_session: bool,
) {
    let original_affinity = ktask::current().cpumask();
    let cpu = khal::percpu::this_cpu_id().as_usize();
    let mut affinity = ktask::KCpuMask::new();
    affinity.set(cpu, true);
    assert!(ktask::set_current_affinity(affinity));
    let parent = kprocess::current_user_thread();
    let parent_mm_id = parent.process().mm_id().unwrap();
    // Keep mapping owners in the surviving parent, never on the dying child stack.
    let mut sentinel = TestUserValue::from_value(0x1234u64).unwrap();
    let status = TestUserValue::from_value(-1i32).unwrap();
    let sentinel_addr = sentinel.as_user_ptr() as usize;
    let prepared = parent
        .prepare_process_fork(ProcessForkConfig {
            parent: ForkParent::Caller,
            address_space: if case.is_private() {
                ForkAddressSpace::Private
            } else {
                ForkAddressSpace::Shared
            },
            fs: ForkFs::Private,
            signal_actions: ForkSignalActions::Private,
            fd_table: ForkFdTable::Private,
            namespace_flags: kns::NamespaceFlags::empty(),
            exit_signal: Some(Signo::SIGCHLD),
        })
        .unwrap();
    let child_pid = prepared.process().pid();
    assert_ne!(child_pid, parent.process().pid());
    assert_eq!(
        prepared.process().mm_id().unwrap() == parent_mm_id,
        !case.is_private()
    );
    {
        let root = executable_root(if case == Case::Malformed {
            b"not ELF".to_vec()
        } else {
            kexec::test_executable_bytes()
        });
        let fs = prepared.process().fs_context().unwrap();
        let mut fs = fs.lock();
        fs.set_root(root.clone()).unwrap();
        fs.set_pwd(root).unwrap();
    }
    let page_table_root = prepared.page_table_root();
    let retirement = if case == Case::Retirement {
        let mm = prepared
            .process()
            .pinned_address_space_for_teardown_observation()
            .unwrap();
        let user = memspace::MmSpace::acquire_user(mm).unwrap();
        let pin = user.pin();
        // Only the child's runtime remains an active user before it can run.
        drop(user);
        Some(Arc::new(Retirement {
            pin,
            is_ready: AtomicBool::new(false),
            can_exec: AtomicBool::new(false),
        }))
    } else {
        None
    };
    let old_mm = retirement
        .as_ref()
        .map(|state| Arc::downgrade(state.pin.address_space()));
    let child_retirement = retirement.clone();
    let (thread, task_number) = prepared.into_parts();
    let has_live_success = Arc::new(AtomicBool::new(false));
    let child_success = has_live_success.clone();
    let switch = Arc::new(PeerSwitch {
        stage: Arc::new(AtomicUsize::new(0)),
        empty_acknowledgement: WaitQueue::new(),
        cpu,
        target_cpu,
    });
    let child_switch = switch.clone();
    let mut task = TaskInner::new_user(
        move || {
            run_child(
                case,
                sentinel_addr,
                &child_success,
                child_switch,
                child_retirement,
                #[cfg(feature = "tee")]
                has_tee_session,
            )
        },
        "exec-lifecycle-child".into(),
        task_number,
        thread,
    );
    task.ctx_mut().set_page_table_root(page_table_root);
    task.set_cpumask(affinity);
    let child = kprocess::start_user_task(task);
    let mut verify_peer_and_ack_fn = |ready, acknowledged| {
        // A blocked child plus same-CPU peer acknowledgement excludes a no-op yield.
        bounded_wait(|| {
            switch.stage.load(Ordering::Acquire) == ready
                && child.state() == ktask::TaskState::Blocked
        });
        assert_eq!(khal::percpu::this_cpu_id().as_usize(), cpu);
        check_current_root(None);
        assert_eq!(parent.process().mm_id().unwrap(), parent_mm_id);
        assert_eq!(sentinel.read(), 0x1234);
        sentinel.write(0x1234);
        switch.stage.store(acknowledged, Ordering::Release);
        if acknowledged == EMPTY_ACKNOWLEDGED {
            switch.empty_acknowledgement.notify_one(false);
        }
    };
    if let Some(retirement) = &retirement {
        retirement.block_release_and_queue_signals(&child, sentinel_addr);
    } else if !case.is_early() && case != Case::Unrelated {
        verify_peer_and_ack_fn(EMPTY_READY, EMPTY_ACKNOWLEDGED);
    }
    if case == Case::Partial {
        verify_peer_and_ack_fn(PARTIAL_READY, PARTIAL_ACKNOWLEDGED);
    }
    bounded_wait(|| child.state() == ktask::TaskState::Exited);
    assert_eq!(
        crate::task::wait::sys_waitpid(child_pid as i32, status.as_user_ptr(), 0),
        Ok(child_pid as isize)
    );
    child.join();
    assert_eq!(
        status.read(),
        if case.is_fatal() {
            Signo::SIGSEGV as i32
        } else {
            0
        }
    );
    // Join completes the child continuation before examining its independent marker.
    assert_eq!(has_live_success.load(Ordering::Relaxed), case.is_success());
    assert_eq!(
        crate::task::wait::sys_waitpid(child_pid as i32, status.as_user_ptr(), 0),
        Err(KError::from(LinuxError::ECHILD))
    );
    assert_eq!(parent.process().mm_id().unwrap(), parent_mm_id);
    assert_eq!(
        sentinel.read(),
        if case.is_early() { 0x5678 } else { 0x1234 }
    );
    sentinel.write(0x9876);
    assert_eq!(sentinel.read(), 0x9876);
    drop(child);
    drop(retirement);
    if let Some(old_mm) = old_mm {
        bounded_wait(|| old_mm.upgrade().is_none());
    }
    assert!(ktask::set_current_affinity(original_affinity));
}

#[def_test(user, serial)]
fn malformed_exec_preserves_shared_mm_and_reaps_child() {
    run_case(Case::Malformed);
}

#[def_test(user, serial)]
fn successful_exec_resets_context_and_reaps_child() {
    run_case(Case::Success);
}

#[def_test(user, serial)]
fn fatal_exec_after_exchange_exits_and_reaps_sigsegv_child() {
    run_case(Case::Fatal);
}

#[def_test(user, serial)]
fn retained_image_survives_real_exec_with_heap_and_private_futex_identity() {
    run_case(Case::RetainedSuccess);
}

#[def_test(user, serial)]
fn replacement_precheck_preserves_shared_mm_and_context() {
    run_case(Case::Precheck);
}

#[def_test(user, serial)]
fn partial_install_failure_switches_reaps_and_spares_unrelated_exec() {
    run_case(Case::Partial);
    run_case(Case::Unrelated);
}

#[def_test(user, serial)]
fn post_update_failure_reaps_and_spares_unrelated_exec() {
    run_case(Case::Update);
    run_case(Case::Unrelated);
}

#[def_test(user, serial)]
fn blocked_last_user_release_preserves_pending_until_reset_and_fatal_exit() {
    run_case(Case::Retirement);
    run_case(Case::Unrelated);
}

#[cfg(feature = "tee")]
#[def_test(user, serial)]
fn tee_session_resources_survive_activation_until_fatal_cleanup() {
    run_case_with_session(Case::Fatal, None, true);
    run_case(Case::Unrelated);
}

#[cfg(feature = "tee")]
#[def_test(user, serial)]
fn tee_session_resources_survive_update_until_fatal_cleanup() {
    run_case_with_session(Case::Update, None, true);
    run_case(Case::Unrelated);
}

#[cfg(feature = "tee")]
#[def_test(user, serial)]
fn tee_session_reset_follows_successful_update() {
    run_case_with_session(Case::Success, None, true);
    run_case(Case::Unrelated);
}

fn run_migration_case(case: Case) -> TestResult {
    let cpu = khal::percpu::this_cpu_id().as_usize();
    let allowed = ktask::current().cpumask();
    let Some(target) = (0..kcpu_id_map::nr_cpus()).find(|id| *id != cpu && allowed.get(*id)) else {
        unittest::ktest_println!("exec migration ignored: fewer than two eligible CPUs");
        return TestResult::Ignored;
    };
    // Fix A after selecting B, so nested fixture setup cannot choose a new origin.
    assert!(ktask::set_current_affinity(ktask::KCpuMask::one_shot(cpu)));
    run_case_with_session(
        case,
        Some(target),
        #[cfg(feature = "tee")]
        false,
    );
    assert!(ktask::set_current_affinity(allowed));
    run_case(Case::Unrelated);
    TestResult::Ok
}

#[def_test(user, serial)]
fn empty_new_mm_executor_migrates_before_fatal_exit() -> TestResult {
    run_migration_case(Case::Fatal)
}

#[def_test(user, serial)]
fn partial_new_mm_executor_migrates_before_fatal_exit() -> TestResult {
    run_migration_case(Case::Partial)
}
