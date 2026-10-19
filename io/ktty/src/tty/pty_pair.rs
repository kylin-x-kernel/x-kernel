// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Original PTY identities, open-file ownership and allocated numbers.

use alloc::{sync::Arc, vec::Vec};

use intrusive_collections::{KeyAdapter, RBTree, RBTreeAtomicLink, intrusive_adapter};
use kerrno::{KError, KResult};
use kprocess::ControllingTerminal;
use kspin::SpinNoPreempt;
use ksync::Mutex;
use kvfs::{DeviceFileOps, DeviceId, VfsFileBuilder, VfsInode, open_character_device};

use super::{PtyDriver, TtyKind, pty::create_endpoints};
use crate::port::InputWork;

const PTY_CAPACITY: u32 = 16;
const UNIX98_PTY_MASTER_MAJOR: u32 = 128;
const UNIX98_PTY_SLAVE_MAJOR: u32 = 136;
const UNIX98_PTY_MINOR_COUNT: u32 = 1 << 20;

/// Returns the fixed system-wide PTY allocation limit.
pub const fn pty_limit() -> usize {
    PTY_CAPACITY as usize
}

/// Returns a task-context snapshot of reserved PTY numbers, not visible nodes.
/// Includes construction and master-closed pairs with surviving slave files.
/// The snapshot does not reserve capacity for a later open.
pub fn pty_allocated_count() -> usize {
    PTYS.lock().allocated.count_ones() as usize
}

// A device number alone cannot identify an original Unix98 PTY instance.
struct UnboundPty;

impl DeviceFileOps for UnboundPty {
    fn open(&self, _inode: &VfsInode, _file: &mut VfsFileBuilder) -> KResult<()> {
        Err(KError::Io)
    }
}

/// Registers Unix98 numeric ranges during device initialization in task context.
/// Masters must use PTMX; slaves must use a node bound to their original pair.
/// A registration conflict rolls back only the ranges installed by this call.
pub fn register_pty_drivers() -> KResult<()> {
    register_pty_ranges(
        DeviceId::new(UNIX98_PTY_MASTER_MAJOR, 0),
        DeviceId::new(UNIX98_PTY_SLAVE_MAJOR, 0),
    )
}

// Keep rollback shared with tests using isolated numbers, not boot-owned ranges.
fn register_pty_ranges(master: DeviceId, slave: DeviceId) -> KResult<()> {
    let operations: Arc<dyn DeviceFileOps> = Arc::new(UnboundPty);
    kvfs::cdev_add(master, UNIX98_PTY_MINOR_COUNT, operations.clone())?;
    if let Err(error) = kvfs::cdev_add(slave, UNIX98_PTY_MINOR_COUNT, operations) {
        drop(kvfs::cdev_del(master, UNIX98_PTY_MINOR_COUNT));
        return Err(error);
    }
    Ok(())
}

/// Allocates, opens and publishes a PTY in sleepable task context.
///
/// The filesystem callback constructs an unpublished slave inode whose opener
/// holds only a weak pair reference. It runs without PTY locks held. Construction
/// failure releases the reservation; later failure is covered by the builder's
/// open-file use. The callback must not publish the inode independently.
pub fn open_ptmx(
    inode: &VfsInode,
    file: &mut VfsFileBuilder,
    create_slave_inode_fn: impl FnOnce(&Arc<PtyPair>) -> KResult<Arc<VfsInode>>,
) -> KResult<()> {
    let pair = PtyPair::allocate()?;
    let slave_inode = create_slave_inode_fn(&pair)?;
    pair.open_master(inode, file)?;
    pair.publish_slave(slave_inode)
}

intrusive_adapter!(PairAdapter = Arc<PtyPair>: PtyPair { link => RBTreeAtomicLink });

impl<'a> KeyAdapter<'a> for PairAdapter {
    type Key = u32;

    fn get_key(&self, pair: &'a PtyPair) -> u32 {
        pair.number
    }
}

struct PtyRegistry {
    pairs: RBTree<PairAdapter>,
    allocated: u64,
}

kspin::static_lock! {
    static PTYS: SpinNoPreempt<PtyRegistry> = SpinNoPreempt::new(PtyRegistry {
        pairs: RBTree::new(PairAdapter::NEW),
        allocated: 0,
    });
}

struct PtyNumber(u32);

impl PtyNumber {
    fn reserve() -> KResult<Self> {
        let mut registry = PTYS.lock();
        let number = registry.allocated.trailing_ones();
        if number >= PTY_CAPACITY {
            return Err(KError::StorageFull);
        }
        registry.allocated |= 1 << number;
        Ok(Self(number))
    }
}

impl Drop for PtyNumber {
    fn drop(&mut self) {
        PTYS.lock().allocated &= !(1 << self.0);
    }
}

struct PtyResources {
    master: Arc<PtyDriver>,
    slave: Arc<PtyDriver>,
    input_work: Arc<InputWork>,
    slave_inode: Option<Arc<VfsInode>>,
    // Return the number after releasing this pair's concrete resource owners.
    _number: PtyNumber,
}

struct PairState {
    is_published: bool,
    has_master: bool,
    slave_opens: usize,
    resources: Option<PtyResources>,
}

/// One allocated PTY identity, independent of the lifetime of its device nodes.
///
/// Construction, publication, open and final file release require sleepable
/// task context. The registry never invokes callbacks or drops final resources
/// under its spinlock. Retained nodes must refer to this object weakly.
pub struct PtyPair {
    link: RBTreeAtomicLink,
    number: u32,
    state: Mutex<PairState>,
}

impl PtyPair {
    /// Reserves a number and constructs connected endpoints, initially hidden.
    /// Dropping an unopened pair rolls back the reservation and endpoint work.
    /// Returns `ENOSPC` when all numbers are allocated; it never waits for space.
    fn allocate() -> KResult<Arc<Self>> {
        let number = PtyNumber::reserve()?;
        Ok(Arc::new_cyclic(|weak| {
            let (master, slave) = create_endpoints(Some(weak.clone()));
            master.set_pty_number(number.0);
            #[expect(
                clippy::expect_used,
                reason = "CG-021-no-panic-on-reachable-failure: create_endpoints builds the slave \
                          with ProcessMode::Work, which always has input work"
            )]
            let input_work = slave
                .input
                .lock()
                .input_work()
                .expect("PTY uses input work");
            Self {
                link: RBTreeAtomicLink::new(),
                number: number.0,
                state: Mutex::new(PairState {
                    is_published: false,
                    has_master: false,
                    slave_opens: 0,
                    resources: Some(PtyResources {
                        master,
                        slave,
                        input_work,
                        slave_inode: None,
                        _number: number,
                    }),
                }),
            }
        }))
    }

    /// Returns the character-device identity to project into the slave inode.
    pub fn slave_device_id(&self) -> DeviceId {
        DeviceId::new(UNIX98_PTY_SLAVE_MAJOR, self.number)
    }

    /// Opens the master once through the ordinary character-device adapter.
    /// Builder failure releases the acquired use without a separate abort call.
    fn open_master(&self, inode: &VfsInode, file: &mut VfsFileBuilder) -> KResult<()> {
        let master = self
            .state
            .lock()
            .resources
            .as_ref()
            .ok_or(KError::Io)?
            .master
            .clone();
        open_character_device(inode, file, master)
    }

    /// Opens this original slave while its master is live and published.
    /// A retained inode cannot reopen a retired pair or follow number reuse.
    pub fn open_slave(&self, inode: &VfsInode, file: &mut VfsFileBuilder) -> KResult<()> {
        let slave = self
            .state
            .lock()
            .resources
            .as_ref()
            .ok_or(KError::Io)?
            .slave
            .clone();
        open_character_device(inode, file, slave)
    }

    /// Publishes a persistent slave inode after master open has succeeded.
    /// The inode's opener must hold only a weak reference to this pair.
    /// Returns `BadState` for repeated publication or an absent master use.
    fn publish_slave(self: &Arc<Self>, inode: Arc<VfsInode>) -> KResult<()> {
        let mut state = self.state.lock();
        if state.is_published || !state.has_master {
            return Err(KError::BadState);
        }
        state
            .resources
            .as_mut()
            .ok_or(KError::BadState)?
            .slave_inode = Some(inode);
        state.is_published = true;
        PTYS.lock().pairs.insert(self.clone());
        Ok(())
    }

    pub(super) fn acquire_open(self: &Arc<Self>, kind: TtyKind) -> KResult<PtyOpen> {
        let kind = match kind {
            TtyKind::PtyMaster => PtyEndpointKind::Master,
            TtyKind::Terminal => PtyEndpointKind::Slave,
            TtyKind::VirtualConsole => return Err(KError::NotATty),
        };
        let mut state = self.state.lock();
        if state.resources.is_none() {
            return Err(KError::Io);
        }
        match kind {
            PtyEndpointKind::Master => {
                if state.has_master || state.is_published {
                    return Err(KError::Io);
                }
                state.has_master = true;
            }
            PtyEndpointKind::Slave => {
                if !state.has_master || !state.is_published {
                    return Err(KError::Io);
                }
                state.slave_opens = state
                    .slave_opens
                    .checked_add(1)
                    .ok_or(KError::ResourceBusy)?;
            }
        }
        Ok(PtyOpen {
            pair: self.clone(),
            kind,
        })
    }

    fn release_open(&self, kind: PtyEndpointKind) {
        let (resources, removed, closed_input) = {
            let mut state = self.state.lock();
            let closed_input = if kind == PtyEndpointKind::Master {
                state
                    .resources
                    .as_ref()
                    .map(|resources| resources.master.ingress.clone())
            } else {
                None
            };
            match kind {
                PtyEndpointKind::Master => state.has_master = false,
                PtyEndpointKind::Slave => state.slave_opens -= 1,
            }
            if state.has_master || state.slave_opens != 0 {
                (None, None, closed_input)
            } else {
                let removed = if state.is_published {
                    let mut registry = PTYS.lock();
                    let mut cursor = registry.pairs.find_mut(&self.number);
                    // The number remains reserved until the detached resources drop.
                    debug_assert!(cursor.get().is_some_and(|pair| core::ptr::eq(pair, self)));
                    cursor.remove()
                } else {
                    None
                };
                state.is_published = false;
                (state.resources.take(), removed, closed_input)
            }
        };
        if let Some(input) = closed_input {
            input.close();
        }
        if let Some(resources) = resources {
            resources.input_work.close();
            // A session snapshot is not an open use. Detach only this terminal,
            // while endpoint Arcs still protect the association cleanup.
            let slave = &resources.slave;
            let association = slave.terminal.association.lock();
            if let Some(session) = slave.terminal.job_control.session() {
                let terminal: Arc<dyn ControllingTerminal> = slave.clone();
                session.unset_terminal(&terminal);
                slave
                    .terminal
                    .job_control
                    .clear_session_if_matches(&session);
            }
            drop(association);
            drop(resources);
        }
        drop(removed);
    }

    fn visible_inode(&self) -> Option<Arc<VfsInode>> {
        let state = self.state.lock();
        if !state.has_master || !state.is_published {
            return None;
        }
        state.resources.as_ref()?.slave_inode.clone()
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum PtyEndpointKind {
    Master,
    Slave,
}

/// One use per open file, not per descriptor. Builder rollback and final file
/// destruction both release it; dup/fork share the containing VfsFile.
pub(super) struct PtyOpen {
    pair: Arc<PtyPair>,
    kind: PtyEndpointKind,
}

impl Drop for PtyOpen {
    fn drop(&mut self) {
        self.pair.release_open(self.kind);
    }
}

/// Looks up the persistent inode of a currently visible slave in task context.
pub fn pty_slave_inode(number: u32) -> Option<Arc<VfsInode>> {
    let pair = PTYS.lock().pairs.find(&number).clone_pointer()?;
    pair.visible_inode()
}

/// Snapshots visible slave numbers; concurrent close may invalidate a result.
pub fn pty_slave_numbers() -> Vec<u32> {
    let mut numbers = Vec::with_capacity(PTY_CAPACITY as usize);
    {
        let registry = PTYS.lock();
        numbers.extend(registry.pairs.iter().map(|pair| pair.number));
    }
    numbers.retain(|&number| pty_slave_inode(number).is_some());
    numbers
}

unittest::path_mod_test!(tests, "../../ut-tests/pty_lifecycle.rs");
