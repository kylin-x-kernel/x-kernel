// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Message queue syscalls.

use alloc::{collections::BTreeMap, sync::Arc, vec::Vec};
use core::mem::size_of;

use kerrno::{KError, KResult, LinuxError};
use kprocess::{Pid, current_user_process};
use ksync::{Mutex, static_lock};
use linux_raw_sys::general::*;
use osvm::VirtPtr;
use posix_types::{IpcPerm, UserConstPtr, UserPtr, msgbuf, msginfo, msqid_ds};

use super::{
    IPC_CREAT, IPC_EXCL, IPC_INFO, IPC_PRIVATE, IPC_RMID, IPC_SET, IPC_STAT, MSG_INFO, MSG_STAT,
    current_unix_seconds, has_ipc_permission, next_ipc_id,
};

/// A queued message type and owned payload.
pub struct Message {
    /// Positive message type used by syscall selection.
    pub mtype: i64,
    /// Payload bytes excluding the message-type header.
    pub data: Vec<u8>,
}

/// Message storage and ABI accounting protected by its manager-provided mutex.
pub struct MessageQueue {
    /// User-visible permissions, limits, counts and timestamps.
    pub msqid_ds: msqid_ds,
    /// Messages in enqueue order; update counters when mutating this vector.
    pub messages: Vec<Message>,
    /// Total payload bytes represented by `messages`.
    pub total_bytes: usize,
    /// Whether control operations marked this queue for removal.
    pub mark_removed: bool,
}

impl MessageQueue {
    /// Creates an empty queue with the given owner IDs and default byte limit.
    pub fn new(key: i32, mode: __kernel_mode_t, pid: Pid, uid: u32, gid: u32) -> Self {
        MessageQueue {
            msqid_ds: msqid_ds {
                msg_perm: IpcPerm {
                    key,
                    uid,
                    gid,
                    cuid: uid,
                    cgid: gid,
                    mode,
                    seq: 0,
                    pad: 0,
                    abi_pad: 0,
                    unused0: 0,
                    unused1: 0,
                },
                msg_stime: 0,
                msg_rtime: 0,
                msg_ctime: current_unix_seconds(),
                msg_cbytes: 0,
                msg_qnum: 0,
                msg_qbytes: MSGMNB as __kernel_size_t,
                msg_lspid: pid as __kernel_pid_t,
                msg_lrpid: pid as __kernel_pid_t,
            },
            messages: Vec::new(),
            total_bytes: 0,
            mark_removed: false,
        }
    }

    /// Appends a payload and updates byte/message accounting.
    ///
    /// # Errors
    ///
    /// Returns ENOSPC when the resulting payload bytes exceed `msg_qbytes`.
    pub fn enqueue_message(&mut self, mtype: i64, data: Vec<u8>) -> KResult<()> {
        let data_len = data.len();
        if self.total_bytes + data_len > self.msqid_ds.msg_qbytes as usize {
            return Err(KError::from(LinuxError::ENOSPC));
        }

        let message = Message { mtype, data };

        self.messages.push(message);
        self.total_bytes += data_len;
        self.msqid_ds.msg_cbytes += data_len as __kernel_size_t;
        self.msqid_ds.msg_qnum += 1;

        Ok(())
    }

    /// Returns the first message as its index, type and borrowed payload.
    pub fn find_first_message(&self) -> Option<(usize, i64, &[u8])> {
        self.messages
            .first()
            .map(|message| (0, message.mtype, &message.data[..]))
    }

    /// Returns the first message whose type equals `msgtyp`.
    pub fn find_message_by_type(&self, msgtyp: i64) -> Option<(usize, i64, &[u8])> {
        self.messages
            .iter()
            .enumerate()
            .find(|(_, message)| message.mtype == msgtyp)
            .map(|(index, message)| (index, message.mtype, &message.data[..]))
    }

    /// Returns the first message whose type differs from `msgtyp`.
    pub fn find_message_not_equal(&self, msgtyp: i64) -> Option<(usize, i64, &[u8])> {
        self.messages
            .iter()
            .enumerate()
            .find(|(_, message)| message.mtype != msgtyp)
            .map(|(index, message)| (index, message.mtype, &message.data[..]))
    }

    /// Returns the first message with the lowest type not exceeding `abs_typ`.
    pub fn find_message_less_equal(&self, abs_typ: i64) -> Option<(usize, i64, &[u8])> {
        let mut candidate = None;

        for (index, message) in self.messages.iter().enumerate() {
            if message.mtype <= abs_typ
                && candidate
                    .as_ref()
                    .is_none_or(|(_, candidate_type, _): &(usize, i64, &[u8])| {
                        message.mtype < *candidate_type
                    })
            {
                candidate = Some((index, message.mtype, &message.data[..]));
            }
        }

        candidate
    }

    /// Returns the number of stored messages, including a removed queue.
    pub fn get_total_message_count(&self) -> usize {
        self.messages.len()
    }

    /// Borrows the message at an enqueue-order index, or returns `None`.
    pub fn get_message_by_index(&self, index: usize) -> Option<&Message> {
        self.messages.get(index)
    }

    /// Removes an indexed message and subtracts its bytes from ABI accounting.
    ///
    /// # Errors
    ///
    /// Returns ENOMSG when `index` is outside the current message vector.
    pub fn remove_message_by_index(&mut self, index: usize) -> KResult<Message> {
        if index < self.messages.len() {
            let removed_msg = self.messages.remove(index);
            self.total_bytes -= removed_msg.data.len();
            self.msqid_ds.msg_cbytes -= removed_msg.data.len() as __kernel_size_t;
            self.msqid_ds.msg_qnum -= 1;
            return Ok(removed_msg);
        }

        Err(KError::from(LinuxError::ENOMSG))
    }
}

/// Global key/ID directory for mutex-protected message queues.
pub struct MsgManager {
    key_msqid: BTreeMap<i32, i32>,
    msqid_queues: BTreeMap<i32, Arc<Mutex<MessageQueue>>>,
}

impl MsgManager {
    const fn new() -> Self {
        MsgManager {
            key_msqid: BTreeMap::new(),
            msqid_queues: BTreeMap::new(),
        }
    }

    /// Iterates all stored IDs and queue references, including removed queues.
    pub fn iter_msg_queues(&self) -> impl Iterator<Item = (i32, &Arc<Mutex<MessageQueue>>)> {
        self.msqid_queues.iter().map(|(&k, v)| (k, v))
    }

    /// Iterates unremoved queues, taking each queue mutex to check its marker.
    pub fn iter_active_queues(&self) -> impl Iterator<Item = (i32, &Arc<Mutex<MessageQueue>>)> {
        self.iter_msg_queues().filter(|(_, queue)| {
            let guard = queue.lock();
            !guard.mark_removed
        })
    }

    /// Looks up the queue ID registered for a key.
    pub fn get_msqid_by_key(&self, key: i32) -> Option<i32> {
        self.key_msqid.get(&key).cloned()
    }

    /// Clones the stored queue reference for an ID, including removed queues.
    pub fn get_queue_by_msqid(&self, msqid: i32) -> Option<Arc<Mutex<MessageQueue>>> {
        self.msqid_queues.get(&msqid).cloned()
    }

    /// Registers or replaces a key-to-ID association.
    pub fn insert_key_msqid(&mut self, key: i32, msqid: i32) {
        self.key_msqid.insert(key, msqid);
    }

    /// Stores or replaces a queue reference for an ID.
    pub fn insert_msqid_queues(&mut self, msqid: i32, msg_queue: Arc<Mutex<MessageQueue>>) {
        self.msqid_queues.insert(msqid, msg_queue);
    }

    /// Counts all stored queue entries, including entries marked removed.
    pub fn queue_count(&self) -> usize {
        self.msqid_queues.len()
    }

    /// Removes an ID and every key association pointing to it.
    pub fn remove_msqid(&mut self, msqid: i32) {
        self.key_msqid.retain(|_, &mut v| v != msqid);
        self.msqid_queues.remove(&msqid);
    }

    /// Sums payload bytes in active queues while taking their mutexes.
    pub fn total_bytes(&self) -> usize {
        self.iter_active_queues()
            .map(|(_, queue)| {
                let guard = queue.lock();
                guard.total_bytes
            })
            .sum()
    }
}

/// Maximum number of queue entries accepted by `sys_msgget`.
pub const MSGMNI: usize = 32000;
/// Default queue payload limit in bytes.
pub const MSGMNB: usize = 16384;
/// Maximum payload bytes accepted by `sys_msgsnd`.
pub const MSGMAX: usize = 8192;

static_lock! {
    /// Global message directory; acquire this mutex before queue mutexes.
    pub static MSG_MANAGER: Mutex<MsgManager> = Mutex::new(MsgManager::new());
}

bitflags::bitflags! {
    /// Receive selection, truncation and nonblocking request flags.
    #[derive(Debug)]
    pub struct MsgRcvFlags: i32 {
        /// Return immediately when no transfer can proceed.
        const IPC_NOWAIT = 0o4000;
        /// Truncate an oversized receive payload.
        const MSG_NOERROR = 0o10000;
        /// Copy an indexed message without removing it.
        const MSG_COPY = 0o20000;
        /// Select a message with a different positive type.
        const MSG_EXCEPT = 0o2000;
    }
}

bitflags::bitflags! {
    /// Message-send nonblocking request flags.
    #[derive(Debug)]
    pub struct MsgSndFlags: i32 {
        /// Return immediately when no transfer can proceed.
        const IPC_NOWAIT = 0o4000;
    }
}

/// Looks up or creates a queue and returns its ID in current-process context.
///
/// # Errors
///
/// Returns ENOSPC at the queue limit, ENOENT for an absent key without create
/// or a missing mapped queue, EEXIST for create/exclusive collision, and EIDRM
/// for a removed queue. The permission branch returns EACCES, but currently
/// uses UID/GID zero rather than the current credentials.
pub fn sys_msgget(key: i32, msgflg: i32) -> KResult<isize> {
    let current_uid: u32 = 0;
    let current_gid: u32 = 0;
    let current_pid = current_user_process().pid();

    let mut msg_manager = MSG_MANAGER.lock();

    if msg_manager.queue_count() >= MSGMNI {
        return Err(KError::from(LinuxError::ENOSPC));
    }

    if key == IPC_PRIVATE {
        let msqid = next_ipc_id();
        let msg_queue = Arc::new(Mutex::new(MessageQueue::new(
            key,
            (msgflg & 0o777) as _,
            current_pid,
            current_uid,
            current_gid,
        )));

        msg_manager.insert_msqid_queues(msqid, msg_queue);
        return Ok(msqid as isize);
    }

    if let Some(msqid) = msg_manager.get_msqid_by_key(key) {
        let msg_queue = msg_manager
            .get_queue_by_msqid(msqid)
            .ok_or(KError::from(LinuxError::ENOENT))?;

        let msg_queue = msg_queue.lock();

        if !has_ipc_permission(
            &msg_queue.msqid_ds.msg_perm,
            current_uid,
            current_gid,
            false,
        ) {
            return Err(KError::from(LinuxError::EACCES));
        }

        if msg_queue.mark_removed {
            return Err(KError::from(LinuxError::EIDRM));
        }

        if (msgflg & IPC_EXCL) != 0 && (msgflg & IPC_CREAT) != 0 {
            return Err(KError::from(LinuxError::EEXIST));
        }

        return Ok(msqid as isize);
    }

    if (msgflg & IPC_CREAT) == 0 {
        return Err(KError::from(LinuxError::ENOENT));
    }

    let msqid = next_ipc_id();
    let msg_queue = Arc::new(Mutex::new(MessageQueue::new(
        key,
        (msgflg & 0o777) as _,
        current_pid,
        current_uid,
        current_gid,
    )));

    msg_manager.insert_key_msqid(key, msqid);
    msg_manager.insert_msqid_queues(msqid, msg_queue);

    Ok(msqid as isize)
}

/// Copies a user message into a queue and returns zero.
///
/// `msgsz` excludes the type header. Requires sleepable current-process context;
/// a full queue returns immediately even without IPC_NOWAIT.
///
/// # Errors
///
/// Returns EINVAL for excessive size, an unknown ID or nonpositive type;
/// EAGAIN when byte/message capacity is exceeded; and forwards user-copy
/// errors and `MessageQueue::enqueue_message` errors. The permission branch
/// returns EACCES, but currently evaluates hard-coded UID/GID zero.
pub fn sys_msgsnd(
    msqid: i32,
    msgp: UserConstPtr<msgbuf>,
    msgsz: usize,
    msgflg: i32,
) -> KResult<isize> {
    if msgsz > MSGMAX {
        return Err(KError::from(LinuxError::EINVAL));
    }
    let current_uid: u32 = 0;
    let current_gid: u32 = 0;
    let current_pid = current_user_process().pid();
    let flags = MsgSndFlags::from_bits_truncate(msgflg);

    let msg_queue = {
        let msg_manager = MSG_MANAGER.lock();
        msg_manager
            .get_queue_by_msqid(msqid)
            .ok_or(KError::from(LinuxError::EINVAL))
    }?;

    let mut msg_queue = msg_queue.lock();

    if !has_ipc_permission(
        &msg_queue.msqid_ds.msg_perm,
        current_uid as _,
        current_gid as _,
        true,
    ) {
        return Err(KError::from(LinuxError::EACCES));
    }

    let mtype_ptr = msgp.cast::<i64>();
    let mtype: i64 = mtype_ptr.read_vm()?;

    if mtype <= 0 {
        return Err(KError::from(LinuxError::EINVAL));
    }

    let data_ptr = UserConstPtr::<u8>::from(msgp.as_ptr() as usize + size_of::<i64>());
    let data_vec = data_ptr.load_vm_vec(msgsz)?;

    let would_exceed_bytes =
        msg_queue.total_bytes + data_vec.len() > msg_queue.msqid_ds.msg_qbytes as usize;
    let would_exceed_messages =
        (msg_queue.msqid_ds.msg_qnum + 1) as usize > msg_queue.msqid_ds.msg_qbytes as usize;

    if would_exceed_bytes || would_exceed_messages {
        if flags.contains(MsgSndFlags::IPC_NOWAIT) {
            return Err(KError::from(LinuxError::EAGAIN));
        }

        warn!("sys_msgsnd: blocking send not implemented, returning EAGAIN");

        return Err(KError::from(LinuxError::EAGAIN));
    }

    msg_queue.enqueue_message(mtype, data_vec)?;

    msg_queue.msqid_ds.msg_lspid = current_pid as _;

    msg_queue.msqid_ds.msg_stime = current_unix_seconds();

    warn!("sys_msgsnd: wakeup of waiting receivers not implemented");
    Ok(0)
}

/// Copies a selected queue message to userspace and returns payload bytes copied.
///
/// `msgtyp` selects FIFO (zero), exact/excluded type (positive), lowest type
/// within an upper bound (negative), or an index with MSG_COPY. Requires
/// sleepable current-process context. Empty queues do not block.
///
/// # Errors
///
/// Returns EINVAL for unsupported MSG_COPY combinations or an unknown ID,
/// EIDRM for a removed queue, ENOMSG for no match/index, and E2BIG when the
/// message exceeds `msgsz` without MSG_NOERROR. Forwards user-copy and indexed
/// removal errors. EACCES is a permission branch using hard-coded zero IDs.
pub fn sys_msgrcv(
    msqid: i32,
    msgp: UserPtr<msgbuf>,
    msgsz: usize,
    msgtyp: i64,
    msgflg: i32,
) -> KResult<isize> {
    let flags = MsgRcvFlags::from_bits_truncate(msgflg);
    let current_uid: u32 = 0;
    let current_gid: u32 = 0;
    let current_pid = current_user_process().pid();

    if flags.contains(MsgRcvFlags::MSG_COPY) {
        if !flags.contains(MsgRcvFlags::IPC_NOWAIT) {
            return Err(KError::from(LinuxError::EINVAL));
        }
        if flags.contains(MsgRcvFlags::MSG_EXCEPT) {
            return Err(KError::from(LinuxError::EINVAL));
        }
    }

    let msg_queue = {
        let msg_manager = MSG_MANAGER.lock();
        msg_manager
            .get_queue_by_msqid(msqid)
            .ok_or(KError::from(LinuxError::EINVAL))
    }?;

    let mut msg_queue = msg_queue.lock();

    if !has_ipc_permission(
        &msg_queue.msqid_ds.msg_perm,
        current_uid as _,
        current_gid as _,
        false,
    ) {
        return Err(KError::from(LinuxError::EACCES));
    }

    if msg_queue.mark_removed {
        return Err(KError::from(LinuxError::EIDRM));
    }

    let (mtype, data_slice, index, should_remove) = if flags.contains(MsgRcvFlags::MSG_COPY) {
        let index = msgtyp as usize;

        if index >= msg_queue.get_total_message_count() {
            return Err(KError::from(LinuxError::ENOMSG));
        }

        let message = msg_queue
            .get_message_by_index(index)
            .ok_or(KError::from(LinuxError::ENOMSG))?;

        (message.mtype, &message.data[..], index, false)
    } else {
        let matched_message = match msgtyp {
            0 => msg_queue.find_first_message(),
            typ if typ > 0 => {
                if flags.contains(MsgRcvFlags::MSG_EXCEPT) {
                    msg_queue.find_message_not_equal(typ)
                } else {
                    msg_queue.find_message_by_type(typ)
                }
            }
            typ if typ < 0 => {
                let abs_typ = typ.abs();
                msg_queue.find_message_less_equal(abs_typ)
            }
            _ => None,
        };

        match matched_message {
            Some((index, mtype, data_slice)) => (mtype, data_slice, index, true),
            None => {
                if flags.contains(MsgRcvFlags::IPC_NOWAIT) {
                    return Err(KError::from(LinuxError::ENOMSG));
                }

                warn!("sys_msgrcv: blocking receive not implemented, returning ENOMSG");
                return Err(KError::from(LinuxError::ENOMSG));
            }
        }
    };

    if data_slice.len() > msgsz {
        if flags.contains(MsgRcvFlags::MSG_NOERROR) {
            // truncate
        } else {
            return Err(KError::from(LinuxError::E2BIG));
        }
    }

    let mtype_ptr = msgp.cast::<i64>();
    mtype_ptr.write_vm(mtype)?;

    let data_ptr = UserPtr::<u8>::from(msgp.as_ptr() as usize + size_of::<i64>());
    let copy_len = data_slice.len().min(msgsz);
    data_ptr.write_vm_slice(&data_slice[..copy_len])?;

    if should_remove {
        msg_queue.remove_message_by_index(index)?;
    }

    if should_remove {
        msg_queue.msqid_ds.msg_lrpid = current_pid as _;
        msg_queue.msqid_ds.msg_rtime = current_unix_seconds();

        warn!("sys_msgrcv: wakeup of waiting senders not implemented");
    } else {
        msg_queue.msqid_ds.msg_lrpid = current_pid as _;
        msg_queue.msqid_ds.msg_rtime = current_unix_seconds();
    }

    Ok(copy_len as isize)
}

/// Queries, updates or marks queue metadata for removal.
///
/// `buf` points to the ABI structure selected by `cmd`; metadata copying may
/// block. Permission checks currently treat the caller as UID/GID zero.
///
/// # Errors
///
/// Returns EINVAL for unsupported commands/IDs/stat indices, EIDRM for a
/// removed queue, EACCES on a failed read permission check, and EPERM for
/// failed owner/creator/privilege or byte-limit checks. Forwards ABI copy errors.
#[expect(
    clippy::too_many_lines,
    reason = "CG-028-focused-functions: TODO(cg): legacy msgctl; split per command"
)]
pub fn sys_msgctl(msqid: i32, cmd: i32, buf: UserPtr<u8>) -> KResult<isize> {
    let current_uid: u32 = 0;
    let current_gid: u32 = 0;
    let is_privileged = current_uid == 0;

    if cmd != IPC_STAT
        && cmd != IPC_SET
        && cmd != IPC_RMID
        && cmd != IPC_INFO
        && cmd != MSG_INFO
        && cmd != MSG_STAT
    {
        return Err(KError::from(LinuxError::EINVAL));
    }

    if cmd == IPC_INFO {
        let info = msginfo {
            msgpool: 0,
            msgmap: 0,
            msgmax: MSGMAX as i32,
            msgmnb: MSGMNB as i32,
            msgmni: MSGMNI as i32,
            msgssz: 0,
            msgtql: 0,
            msgseg: 0,
            pad: 0,
        };

        let ptr = buf.cast::<msginfo>();
        ptr.write_vm(info)?;
        return Ok(0);
    }

    if cmd == MSG_INFO {
        let msg_manager = MSG_MANAGER.lock();
        let msg_perm = IpcPerm {
            key: 0,
            uid: current_uid,
            gid: current_gid,
            cuid: current_uid,
            cgid: current_gid,
            mode: 0o600,
            pad: 0,
            seq: 0,
            abi_pad: 0,
            unused0: 0,
            unused1: 0,
        };

        let info_ds = msqid_ds {
            msg_perm,
            msg_stime: 0,
            msg_rtime: 0,
            msg_ctime: 0,
            msg_cbytes: msg_manager.total_bytes() as u64,
            msg_qnum: msg_manager.queue_count() as u64,
            msg_qbytes: MSGMNB as u64,
            msg_lspid: Pid::from(0u32) as _,
            msg_lrpid: Pid::from(0u32) as _,
        };

        let ptr = buf.cast::<msqid_ds>();
        ptr.write_vm(info_ds)?;

        return Ok(msg_manager.queue_count() as isize);
    }

    if cmd == MSG_STAT {
        let msg_manager = MSG_MANAGER.lock();

        let result = msg_manager
            .iter_active_queues()
            .nth(msqid as usize)
            .ok_or(KError::from(LinuxError::EINVAL))
            .and_then(|(actual_msqid, queue)| {
                let guard = queue.lock();

                if !has_ipc_permission(&guard.msqid_ds.msg_perm, current_uid, current_gid, false) {
                    return Err(KError::from(LinuxError::EACCES));
                }

                let ptr = buf.cast::<msqid_ds>();
                ptr.write_vm(guard.msqid_ds)?;
                Ok(actual_msqid as isize)
            });

        return result;
    }

    let msg_queue = {
        let msg_manager = MSG_MANAGER.lock();
        msg_manager
            .get_queue_by_msqid(msqid)
            .ok_or(KError::from(LinuxError::EINVAL))
    }?;

    let mut msg_queue = msg_queue.lock();
    if msg_queue.mark_removed {
        return Err(KError::from(LinuxError::EIDRM));
    }
    if cmd == IPC_STAT {
        if !has_ipc_permission(
            &msg_queue.msqid_ds.msg_perm,
            current_uid,
            current_gid,
            false,
        ) {
            return Err(KError::from(LinuxError::EACCES));
        }

        let ptr = buf.cast::<msqid_ds>();
        ptr.write_vm(msg_queue.msqid_ds)?;

        return Ok(0);
    }

    let is_owner = current_uid == msg_queue.msqid_ds.msg_perm.uid;
    let is_creator = current_uid == msg_queue.msqid_ds.msg_perm.cuid;

    if !is_privileged && !is_owner && !is_creator {
        return Err(KError::from(LinuxError::EPERM));
    }

    if cmd == IPC_SET {
        let user_buf = buf.cast::<msqid_ds>().read_vm()?;

        msg_queue.msqid_ds.msg_perm.uid = user_buf.msg_perm.uid;
        msg_queue.msqid_ds.msg_perm.gid = user_buf.msg_perm.gid;
        msg_queue.msqid_ds.msg_perm.mode = user_buf.msg_perm.mode & 0o777;

        if user_buf.msg_qbytes != msg_queue.msqid_ds.msg_qbytes {
            if user_buf.msg_qbytes > MSGMNB as _ && !is_privileged {
                return Err(KError::from(LinuxError::EPERM));
            }
            msg_queue.msqid_ds.msg_qbytes = user_buf.msg_qbytes;
        }

        msg_queue.msqid_ds.msg_ctime = current_unix_seconds();

        return Ok(0);
    }
    if cmd == IPC_RMID {
        msg_queue.mark_removed = true;

        if msg_queue.msqid_ds.msg_qnum == 0 {
            drop(msg_queue);

            MSG_MANAGER.lock().remove_msqid(msqid);

            warn!(
                "sys_msgctl[IPC_RMID]: wakeup of waiting processes after queue deletion not \
                 implemented"
            );

            return Ok(0);
        }

        msg_queue.msqid_ds.msg_ctime = current_unix_seconds();

        return Ok(0);
    }
    Err(KError::from(LinuxError::EINVAL))
}

#[unittest::mod_test]
mod tests_msg {
    use alloc::vec;

    use unittest::def_test;

    use super::*;

    #[def_test]
    fn test_message_queue_preserves_fifo_order_for_plain_receive() {
        let mut queue = MessageQueue::new(1, 0o600, 1, 0, 0);
        queue.enqueue_message(2, b"first".to_vec()).unwrap();
        queue.enqueue_message(1, b"second".to_vec()).unwrap();

        let (index, mtype, data) = queue.find_first_message().expect("first queued message");
        assert_eq!(index, 0);
        assert_eq!(mtype, 2);
        assert_eq!(data, b"first");
    }

    #[def_test]
    fn test_message_queue_type_selection_matches_linux_basics() {
        let mut queue = MessageQueue::new(2, 0o600, 1, 0, 0);
        queue.enqueue_message(5, b"type5-a".to_vec()).unwrap();
        queue.enqueue_message(2, b"type2".to_vec()).unwrap();
        queue.enqueue_message(5, b"type5-b".to_vec()).unwrap();
        queue.enqueue_message(3, b"type3".to_vec()).unwrap();

        let (_, exact_type, exact_data) = queue.find_message_by_type(5).expect("type 5");
        assert_eq!(exact_type, 5);
        assert_eq!(exact_data, b"type5-a");

        let (_, except_type, except_data) = queue.find_message_not_equal(5).expect("type != 5");
        assert_eq!(except_type, 2);
        assert_eq!(except_data, b"type2");

        let (_, le_type, le_data) = queue.find_message_less_equal(4).expect("type <= 4");
        assert_eq!(le_type, 2);
        assert_eq!(le_data, b"type2");
    }

    #[def_test]
    fn test_message_queue_remove_updates_accounting() {
        let mut queue = MessageQueue::new(3, 0o600, 1, 0, 0);
        queue.enqueue_message(1, vec![1, 2, 3]).unwrap();
        queue.enqueue_message(2, vec![4, 5]).unwrap();

        assert_eq!(queue.get_total_message_count(), 2);
        assert_eq!(queue.total_bytes, 5);
        assert_eq!(queue.msqid_ds.msg_qnum, 2);

        let removed = queue.remove_message_by_index(0).expect("remove first");
        assert_eq!(removed.mtype, 1);
        assert_eq!(removed.data, vec![1, 2, 3]);
        assert_eq!(queue.get_total_message_count(), 1);
        assert_eq!(queue.total_bytes, 2);
        assert_eq!(queue.msqid_ds.msg_qnum, 1);
        assert_eq!(queue.msqid_ds.msg_cbytes, 2);
    }
}
