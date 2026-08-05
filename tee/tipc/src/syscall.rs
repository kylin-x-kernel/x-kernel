// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! TIPC syscall ABI adapters.

use alloc::{sync::Arc, vec::Vec};
use core::{any::Any, ffi::c_char, future::poll_fn, task::Poll};

use kerrno::{KError, KResult};
use khal::{paging::MappingFlags, uspace::UserContext};
use kpoll::{PollContext, PollRegisterError, PollRegistrations};
use linux_sysno::Sysno;
use memaddr::VirtAddr;
use posix_types::{IoVec, UserConstPtr, UserPtr};
use uuid::Uuid;

use crate::{
    Handle, HandleEventMask, HandleKind, HandleSet, HandleSetCommand, HandleSetEntry,
    IPC_CHAN_MAX_BUF_SIZE, IPC_MAX_MSG_HANDLES, IpcChan, IpcConnectFlags, IpcPort, IpcPortFlags,
    IpcUuid, MemRef, UEvent, ipc_port_connect_async, ipc_port_create, ipc_port_publish,
};

#[derive(Clone, Copy, posix_types::UserRead)]
#[repr(C)]
pub(crate) struct IpcMsg {
    num_iov: u32,
    iov: usize,
    num_handles: u32,
    handles: usize,
}

#[derive(Clone, Copy, posix_types::UserWrite)]
#[repr(C)]
pub(crate) struct IpcMsgInfo {
    len: usize,
    id: u32,
    num_handles: u32,
}

#[derive(Clone, Copy, posix_types::UserRead, posix_types::UserWrite)]
#[repr(C)]
pub(crate) struct UserEvent {
    handle: i32,
    event: u32,
    cookie: usize,
}

impl From<UEvent> for UserEvent {
    fn from(event: UEvent) -> Self {
        Self {
            handle: event.handle,
            event: event.event.bits(),
            cookie: event.cookie,
        }
    }
}

/// TIPC UUID ABI carrier written back to rust-libtipc callers.
///
/// This type is `#[repr(transparent)]` over `uuid::Uuid` so the syscall ABI
/// exposes the same 16-byte RFC 4122 byte layout as rust-libtipc's transparent
/// `TipcUuid(Uuid)` wrapper. The size and alignment assertions below keep that
/// userspace contract explicit.
///
/// The field is private because `IpcUuid` is the kernel semantic type. Syscall
/// code should cross the ABI boundary through `From<IpcUuid>` instead of
/// constructing raw ABI carriers directly.
#[derive(Clone, Copy, posix_types::UserWrite)]
#[repr(transparent)]
pub(crate) struct TipcUuid(Uuid);

const _: () = assert!(core::mem::size_of::<TipcUuid>() == 16);
const _: () = assert!(core::mem::align_of::<TipcUuid>() == 1);

impl From<IpcUuid> for TipcUuid {
    fn from(uuid: IpcUuid) -> Self {
        Self(uuid.into_uuid())
    }
}

/// Dispatches one TIPC syscall from the userspace context.
pub fn dispatch_irq_tipc_syscall(sysno: Sysno, uctx: &mut UserContext) -> KResult<isize> {
    match sysno {
        Sysno::tipc_port_create => sys_tipc_port_create(
            uctx.arg0().into(),
            uctx.arg1() as _,
            uctx.arg2() as _,
            uctx.arg3() as _,
        ),
        Sysno::tipc_connect => sys_tipc_connect(uctx.arg0().into(), uctx.arg1() as _),
        Sysno::tipc_accept => sys_tipc_accept(uctx.arg0() as _, uctx.arg1().into()),
        Sysno::tipc_close => sys_tipc_close(uctx.arg0() as _),
        Sysno::tipc_set_cookie => sys_tipc_set_cookie(uctx.arg0() as _, uctx.arg1()),
        Sysno::tipc_handle_set_create => sys_tipc_handle_set_create(),
        Sysno::tipc_handle_set_ctrl => {
            sys_tipc_handle_set_ctrl(uctx.arg0() as _, uctx.arg1() as _, uctx.arg2().into())
        }
        Sysno::tipc_wait => sys_tipc_wait(uctx.arg0() as _, uctx.arg1().into(), uctx.arg2() as _),
        Sysno::tipc_wait_any => sys_tipc_wait_any(uctx.arg0().into(), uctx.arg1() as _),
        Sysno::tipc_get_msg => sys_tipc_get_msg(uctx.arg0() as _, uctx.arg1().into()),
        Sysno::tipc_read_msg => sys_tipc_read_msg(
            uctx.arg0() as _,
            uctx.arg1() as _,
            uctx.arg2() as _,
            uctx.arg3().into(),
        ),
        Sysno::tipc_put_msg => sys_tipc_put_msg(uctx.arg0() as _, uctx.arg1() as _),
        Sysno::tipc_send_msg => sys_tipc_send_msg(uctx.arg0() as _, uctx.arg1().into()),
        Sysno::tipc_memref_create => {
            sys_tipc_memref_create(uctx.arg0(), uctx.arg1() as _, uctx.arg2() as _)
        }
        _ => Err(KError::Unsupported),
    }
}

fn install(handle: Arc<dyn Handle>) -> KResult<isize> {
    kprocess::current_user_process()
        .with_tipc_handles(|handles| handles.write().uctx_handle_install(handle))?
        .map(|id| id as isize)
}

fn get_handle(id: i32) -> KResult<Arc<dyn Handle>> {
    kprocess::current_user_process()
        .with_tipc_handles(|handles| handles.read().uctx_handle_get(id))?
}

fn get_as<T: Handle + 'static>(id: i32) -> KResult<Arc<T>> {
    let handle: Arc<dyn Any + Send + Sync> = get_handle(id)?;
    handle.downcast::<T>().map_err(|_| KError::InvalidInput)
}

fn handle_set_command(command: u32) -> KResult<HandleSetCommand> {
    match command {
        crate::HSET_ADD => Ok(HandleSetCommand::Add),
        crate::HSET_DEL => Ok(HandleSetCommand::Delete),
        crate::HSET_MOD => Ok(HandleSetCommand::Modify),
        crate::HSET_DEL_GET_COOKIE => Ok(HandleSetCommand::DeleteGetCookie),
        crate::HSET_DEL_WITH_COOKIE => Ok(HandleSetCommand::DeleteWithCookie),
        crate::HSET_MOD_WITH_COOKIE => Ok(HandleSetCommand::ModifyWithCookie),
        _ => Err(KError::InvalidInput),
    }
}

fn timeout_duration(timeout_ms: u32) -> Option<ktime_types::TimeSpan> {
    (timeout_ms != u32::MAX).then(|| ktime_types::TimeSpan::from_millis(timeout_ms as u64))
}

fn wait_for_event(
    timeout_ms: u32,
    mut wait: impl FnMut(&mut PollContext<'_>) -> Poll<KResult<UserEvent>>,
) -> KResult<UserEvent> {
    let mut registrations = PollRegistrations::new();
    ktask::future::block_on(ktask::future::timeout(
        timeout_duration(timeout_ms),
        poll_fn(move |cx| {
            let mut context = registrations.context(cx);
            wait(&mut context)
        }),
    ))
    .map_err(KError::from)?
}

fn map_register_error(error: PollRegisterError) -> KError {
    match error {
        PollRegisterError::NoMemory | PollRegisterError::IdExhausted => KError::NoMemory,
        PollRegisterError::InvalidState => KError::InvalidInput,
    }
}

fn poll_handle_event(handle_id: i32, handle: &Arc<dyn Handle>) -> Option<UserEvent> {
    let event = handle.poll(true);
    (!event.is_empty()).then_some(UserEvent {
        handle: handle_id,
        event: event.bits(),
        cookie: handle.cookie(),
    })
}

fn poll_handle_or_set_event(
    handle_id: i32,
    handle: &Arc<dyn Handle>,
) -> KResult<Option<UserEvent>> {
    if let Some(hset) = handle.as_any().downcast_ref::<HandleSet>() {
        return hset.poll_one().map(|event| event.map(Into::into));
    }
    Ok(poll_handle_event(handle_id, handle))
}

fn load_iovecs(msg: IpcMsg) -> KResult<Vec<IoVec>> {
    if msg.num_iov == 0 {
        return Ok(Vec::new());
    }
    IoVec::load_from_user(msg.iov.into(), msg.num_iov as usize)
}

fn copy_iovecs_from_user(iovecs: &[IoVec], max_len: usize) -> KResult<Vec<u8>> {
    let total_len = iovecs.iter().try_fold(0usize, |total, iov| {
        if iov.iov_len < 0 {
            return Err(KError::InvalidInput);
        }
        let len = iov.iov_len as usize;
        if len > max_len.saturating_sub(total) {
            return Err(KError::OutOfRange);
        }
        Ok(total + len)
    })?;

    let mut data = Vec::with_capacity(total_len);
    for iov in iovecs {
        let len = iov.iov_len as usize;
        if len == 0 {
            continue;
        }
        let bytes = UserConstPtr::<u8>::from(iov.iov_base.cast_const()).load_vm_vec(len)?;
        data.try_reserve(bytes.len())
            .map_err(|_| KError::NoMemory)?;
        data.extend_from_slice(&bytes);
    }
    Ok(data)
}

fn iovecs_total_len(iovecs: &[IoVec]) -> KResult<usize> {
    let mut total = 0usize;
    for iov in iovecs {
        if iov.iov_len < 0 {
            return Err(KError::InvalidInput);
        }
        let len = iov.iov_len as usize;
        if len == 0 {
            continue;
        }
        total = total.saturating_add(len).min(IPC_CHAN_MAX_BUF_SIZE);
        if total == IPC_CHAN_MAX_BUF_SIZE {
            break;
        }
    }
    Ok(total)
}

fn copy_iovecs_to_user(iovecs: &[IoVec], data: &[u8]) -> KResult<usize> {
    let mut total = 0usize;
    for iov in iovecs {
        if iov.iov_len < 0 {
            return Err(KError::InvalidInput);
        }
        if total == data.len() {
            break;
        }
        let len = (iov.iov_len as usize).min(data.len() - total);
        if len == 0 {
            continue;
        }
        UserPtr::<u8>::from(iov.iov_base).write_vm_slice(&data[total..total + len])?;
        total += len;
    }
    Ok(total)
}

fn load_msg_handles(msg: IpcMsg) -> KResult<Vec<Arc<dyn Handle>>> {
    if msg.num_handles as usize > IPC_MAX_MSG_HANDLES {
        return Err(KError::OutOfRange);
    }
    if msg.num_handles == 0 {
        return Ok(Vec::new());
    }
    let ids = UserConstPtr::<i32>::from(msg.handles).load_vm_vec(msg.num_handles as usize)?;
    kprocess::current_user_process().with_tipc_handles(|handles| {
        let table = handles.read();
        ids.into_iter()
            .map(|id| table.uctx_handle_get(id))
            .collect()
    })?
}

fn memref_access_flags(mmap_prot: u32) -> MappingFlags {
    let mut flags = MappingFlags::USER;
    if mmap_prot & crate::MMAP_FLAG_PROT_READ != 0 {
        flags |= MappingFlags::READ;
    }
    if mmap_prot & crate::MMAP_FLAG_PROT_WRITE != 0 {
        flags |= MappingFlags::WRITE;
    }
    flags
}

fn validate_current_memref_range(addr: usize, size: usize, mmap_prot: u32) -> KResult {
    let start = VirtAddr::from_usize(addr);
    let flags = memref_access_flags(mmap_prot);
    let address_space = kprocess::current_user_process().address_space()?;
    if !address_space.lock().can_access_range(start, size, flags) {
        return Err(KError::PermissionDenied);
    }
    Ok(())
}

fn return_msg_handles(msg: IpcMsg, handles: Vec<Arc<dyn Handle>>) -> KResult {
    if handles.is_empty() {
        return Ok(());
    }
    if handles.len() > IPC_MAX_MSG_HANDLES {
        return Err(KError::OutOfRange);
    }

    let mut ids = Vec::with_capacity(handles.len());
    {
        kprocess::current_user_process().with_tipc_handles(|handle_table| {
            let mut table = handle_table.write();
            for handle in handles {
                match table.uctx_handle_install(handle) {
                    Ok(id) => ids.push(id),
                    Err(err) => {
                        for id in ids.drain(..) {
                            let _ = table.uctx_handle_uninstall(id);
                        }
                        return Err(err);
                    }
                }
            }
            Ok(())
        })??;
    }

    if let Err(err) = UserPtr::<i32>::from(msg.handles).write_vm_slice(&ids) {
        let _ = kprocess::current_user_process().with_tipc_handles(|handle_table| {
            let mut table = handle_table.write();
            for id in ids {
                let _ = table.uctx_handle_uninstall(id);
            }
        });
        return Err(err.into());
    }
    Ok(())
}

fn read_path(path: UserConstPtr<u8>) -> KResult<alloc::string::String> {
    path.cast::<c_char>()
        .load_string_with_max_len(crate::IPC_PORT_PATH_MAX - 1)
        .map_err(|err| {
            if err == KError::IllegalBytes {
                KError::InvalidData
            } else {
                err
            }
        })
}

#[cfg(feature = "tee")]
fn current_caller_uuid() -> KResult<IpcUuid> {
    let uuid = kprocess::current_user_process().with_tee_ta_ctx(|ctx| ctx.uuid.clone())?;
    let tapp_uuid = Uuid::parse_str(&uuid).map_err(|_| KError::InvalidData)?;
    Ok(IpcUuid::from_uuid(tapp_uuid))
}

#[cfg(not(feature = "tee"))]
fn current_caller_uuid() -> KResult<IpcUuid> {
    Ok(IpcUuid::default())
}

/// Creates and publishes a TIPC service port.
fn sys_tipc_port_create(
    path: UserConstPtr<u8>,
    num_recv_bufs: u32,
    recv_buf_size: u32,
    flags: u32,
) -> KResult<isize> {
    let flags = IpcPortFlags::from_bits(flags).ok_or(KError::InvalidInput)?;
    let port = ipc_port_create(
        current_caller_uuid()?,
        read_path(path)?,
        num_recv_bufs as usize,
        recv_buf_size as usize,
        flags,
    )?;
    let id = install(port.clone())? as i32;
    if let Err(err) = ipc_port_publish(&port) {
        let _ = kprocess::current_user_process()
            .with_tipc_handles(|handles| handles.write().uctx_handle_uninstall(id));
        return Err(err);
    }
    Ok(id as isize)
}

/// Opens a client endpoint to a named TIPC service.
fn sys_tipc_connect(path: UserConstPtr<u8>, flags: u32) -> KResult<isize> {
    let flags = IpcConnectFlags::from_bits(flags).ok_or(KError::InvalidInput)?;
    let channel = ipc_port_connect_async(current_caller_uuid()?, &read_path(path)?, flags)?;
    if !flags.contains(IpcConnectFlags::ASYNC) {
        channel.wait_connected()?;
    }
    install(channel)
}

/// Accepts one pending connection from a service port.
fn sys_tipc_accept(port_id: i32, peer_uuid: UserPtr<TipcUuid>) -> KResult<isize> {
    let port = get_as::<IpcPort>(port_id)?;
    let (channel, peer) = port.ipc_port_accept()?;
    let id = install(channel)? as i32;
    if let Err(err) = peer_uuid.write_vm(peer.into()) {
        let _ = kprocess::current_user_process()
            .with_tipc_handles(|handles| handles.write().uctx_handle_uninstall(id));
        return Err(err.into());
    }
    Ok(id as isize)
}

/// Closes a process-local TIPC handle.
fn sys_tipc_close(handle: i32) -> KResult<isize> {
    kprocess::current_user_process()
        .with_tipc_handles(|handles| handles.write().uctx_handle_remove(handle))??;
    Ok(0)
}

/// Sets an opaque event cookie on a handle.
fn sys_tipc_set_cookie(handle: i32, cookie: usize) -> KResult<isize> {
    let handle = get_handle(handle)?;

    // cookies are only relevant for pollable handles
    if handle.kind() == HandleKind::MemRef {
        return Err(KError::InvalidInput);
    }

    handle.set_cookie(cookie);
    Ok(0)
}

/// Creates an empty TIPC handle set.
fn sys_tipc_handle_set_create() -> KResult<isize> {
    install(HandleSet::handle_set_create())
}

/// Adds, removes, or modifies one handle-set registration.
fn sys_tipc_handle_set_ctrl(
    hset_id: i32,
    command: u32,
    user_event: UserPtr<UserEvent>,
) -> KResult<isize> {
    let hset = get_as::<HandleSet>(hset_id)?;
    let command = handle_set_command(command)?;
    let event = user_event.read_vm()?;
    let handle = get_handle(event.handle)?;
    let event_mask = HandleEventMask::from_bits(event.event).ok_or(KError::InvalidInput)?;
    let entry = HandleSetEntry {
        handle_id: event.handle,
        handle,
        event: event_mask,
        cookie: event.cookie,
    };
    if let Some(cookie) = hset.handle_set_ctrl(command, entry)? {
        user_event.write_vm(UserEvent { cookie, ..event })?;
    }
    Ok(0)
}

/// Waits for one handle, or one child of a handle set, to become ready.
fn sys_tipc_wait(
    handle_id: i32,
    user_event: UserPtr<UserEvent>,
    timeout_ms: u32,
) -> KResult<isize> {
    let handle = get_handle(handle_id)?;
    let event = wait_for_event(timeout_ms, |cx| {
        match poll_handle_or_set_event(handle_id, &handle) {
            Ok(Some(event)) => return Poll::Ready(Ok(event)),
            Ok(None) => {}
            Err(err) => return Poll::Ready(Err(err)),
        }
        if let Err(error) = handle.register(cx, HandleEventMask::READY) {
            return Poll::Ready(Err(map_register_error(error)));
        }
        match poll_handle_or_set_event(handle_id, &handle) {
            Ok(Some(event)) => Poll::Ready(Ok(event)),
            Ok(None) => Poll::Pending,
            Err(err) => Poll::Ready(Err(err)),
        }
    })?;
    user_event.write_vm(event)?;
    Ok(0)
}

/// Waits for any cookie-bearing process-local TIPC handle.
fn sys_tipc_wait_any(user_event: UserPtr<UserEvent>, timeout_ms: u32) -> KResult<isize> {
    let event = wait_for_event(timeout_ms, |cx| {
        let snapshot = match kprocess::current_user_process().with_tipc_handles(|handles| {
            let mut table = handles.write();
            table.wait_any_snapshot()
        }) {
            Ok(snapshot) => snapshot,
            Err(err) => return Poll::Ready(Err(err)),
        };
        if snapshot.is_empty() {
            return Poll::Ready(Err(KError::NotFound));
        }
        for (id, handle) in snapshot.iter() {
            if let Some(event) = poll_handle_event(*id, handle) {
                return Poll::Ready(Ok(event));
            }
        }
        for (_, handle) in snapshot.iter() {
            if let Err(error) = handle.register(cx, HandleEventMask::READY) {
                return Poll::Ready(Err(map_register_error(error)));
            }
        }
        // A handle may be installed after the snapshot was taken. Register for
        // table changes as well, because its readiness cannot be observed by
        // any handle registration above.
        let table_registration = kprocess::current_user_process().with_tipc_handles(|handles| {
            handles
                .read()
                .register_wait_any_table_change(cx)
                .map_err(map_register_error)
        });
        match table_registration {
            Ok(Ok(())) => {}
            Ok(Err(err)) | Err(err) => return Poll::Ready(Err(err)),
        }
        // Re-acquire the cached snapshot and poll once more after all wakers
        // are registered. This closes the check-then-register race for both a
        // pre-existing handle becoming ready and table membership changing.
        let snapshot = match kprocess::current_user_process().with_tipc_handles(|handles| {
            let mut table = handles.write();
            table.wait_any_snapshot()
        }) {
            Ok(snapshot) => snapshot,
            Err(err) => return Poll::Ready(Err(err)),
        };
        if snapshot.is_empty() {
            return Poll::Ready(Err(KError::NotFound));
        }
        for (id, handle) in snapshot.iter() {
            if let Some(event) = poll_handle_event(*id, handle) {
                return Poll::Ready(Ok(event));
            }
        }
        Poll::Pending
    })?;
    user_event.write_vm(event)?;
    Ok(0)
}

/// Claims one incoming message and returns its metadata.
fn sys_tipc_get_msg(handle: i32, user_info: UserPtr<IpcMsgInfo>) -> KResult<isize> {
    let channel = get_as::<IpcChan>(handle)?;
    let info = channel.ipc_peek_next_filled_msg()?;
    user_info.write_vm(IpcMsgInfo {
        len: info.len,
        id: info.id as u32,
        num_handles: info.num_handles,
    })?;
    channel.ipc_get_filled_msg(info.id)?;
    Ok(0)
}

/// Reads bytes from a claimed message into user iovecs.
fn sys_tipc_read_msg(
    handle: i32,
    msg_id: u32,
    offset: u32,
    user_msg: UserConstPtr<IpcMsg>,
) -> KResult<isize> {
    let msg = user_msg.read_vm()?;
    let channel = get_as::<IpcChan>(handle)?;
    if msg.num_handles as usize > IPC_MAX_MSG_HANDLES {
        return Err(KError::OutOfRange);
    }
    let iovecs = load_iovecs(msg)?;
    let max_len = iovecs_total_len(&iovecs)?;
    let read_msg = channel.ipc_read_msg_with_handles(
        msg_id as usize,
        offset as usize,
        max_len,
        msg.num_handles as usize,
    )?;
    let read = copy_iovecs_to_user(&iovecs, &read_msg.data)?;
    return_msg_handles(msg, read_msg.handles)?;
    Ok(read as isize)
}

/// Releases a claimed message slot.
fn sys_tipc_put_msg(handle: i32, msg_id: u32) -> KResult<isize> {
    let channel = get_as::<IpcChan>(handle)?;
    channel.ipc_put_msg(msg_id as usize)?;
    Ok(0)
}

/// Sends one complete message assembled from user iovecs.
fn sys_tipc_send_msg(handle: i32, user_msg: UserConstPtr<IpcMsg>) -> KResult<isize> {
    let channel = get_as::<IpcChan>(handle)?;
    let msg = user_msg.read_vm()?;
    let iovecs = load_iovecs(msg)?;
    let data = copy_iovecs_from_user(&iovecs, channel.peer_recv_buf_size()?)?;
    let handles = load_msg_handles(msg)?;
    channel
        .ipc_send_msg_with_handles(&data, &handles)
        .map(|len| len as isize)
}

/// Creates a transferable TIPC memref handle.
fn sys_tipc_memref_create(addr: usize, size: u32, prot: u32) -> KResult<isize> {
    let size = size as usize;
    let memref = MemRef::create(addr, size, prot)?;
    validate_current_memref_range(addr, size, prot)?;
    install(memref)
}
