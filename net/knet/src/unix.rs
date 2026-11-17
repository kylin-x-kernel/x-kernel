// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Unix domain socket implementations.
pub(crate) mod dgram;
pub(crate) mod seqpacket;
pub(crate) mod stream;

use alloc::sync::Arc;

use enum_dispatch::enum_dispatch;
use hashbrown::HashMap;
use kalloc_types::KBox;
use kerrno::{KError, KResult};
use kio::{IoBuf, IoBufMut, Read, Write};
use klazy::lazy_static;
use kpoll::{IoEvents, PollContext, PollRegisterError, Pollable};
use kprocess::AsThread;
use ksync::Mutex;
use ktask::future::block_on_interruptible;
use kvfs::{
    DeviceId, Filename, LookupFlags, LookupIntent, NodePermission, NodeType, Path, Permission,
    WeakVfsInode,
};

pub use self::{dgram::DgramTransport, seqpacket::SeqpacketTransport, stream::StreamTransport};
use crate::{
    AcceptOptions, ConnectOptions, QueueLengths, RecvOptions, SendOptions, Shutdown, Socket,
    SocketAddrEx, SocketOps,
    options::{Configurable, GetSocketOption, OptionHandled, PeerCredentials, SetSocketOption},
};

/// Captures the current process identity and effective UID/GID as a peer
/// credential snapshot.
///
/// User-facing Unix listener and stream-connect entry points call this when
/// Linux establishes the peer relationship. The PID stays a process identity
/// until a reader resolves it through its own active PID namespace. Kernel
/// callers without a user thread call the inherent
/// [`UnixDomainSocket::listen_with_credentials`] or
/// [`UnixDomainSocket::connect_with_credentials`] on the concrete Unix socket
/// with an explicit snapshot (or [`PeerCredentials::none`]); the `SocketOps`
/// implementations below are the user-facing paths and require a user thread.
///
/// # Execution context
///
/// Must run on a user thread in process context. It reads the current task's
/// subjective credentials through a sleepable `RwLock`, so it must not run in
/// IRQ context, with preemption disabled, or while holding a spinlock; ordinary
/// sleepable locks may be held. It performs no I/O and does not wait for
/// another task.
///
/// # Panics
///
/// Panics when the current task has no user-thread runtime, including a kernel
/// task or an interrupt context.
pub fn current_peer_credentials() -> PeerCredentials {
    let thread = kprocess::current_user_thread();
    let cred = thread.cred();
    PeerCredentials::new(thread.process().pid_identity(), cred.euid(), cred.egid())
}

/// Captures the current sending process's identity and real UID/GID.
///
/// User-facing socket send entry points call this once per send. The pid stays
/// a process identity so each receiver resolves it in its own PID namespace
/// (Linux `pid_vnr` at delivery). Kernel callers without a user thread supply
/// an explicit [`SendOptions::sender_credentials`] or none.
///
/// # Execution context
///
/// Runs on a user thread in process context when one exists. It reads the
/// current task's subjective credentials through a sleepable `RwLock`, so it
/// must not run in IRQ context, with preemption disabled, or while holding a
/// spinlock; ordinary sleepable locks may be held. It performs no I/O and does
/// not wait for another task.
///
/// Returns `None` for a kernel task or interrupt context, which have no
/// user-visible sender identity.
pub fn current_sender_credentials() -> Option<crate::SenderCredentials> {
    let current = ktask::current();
    let thread = current.try_as_thread()?;
    let cred = kprocess::current_cred();
    Some(crate::SenderCredentials {
        pid: Some(thread.process().pid_identity()),
        uid: cred.ruid(),
        gid: cred.rgid(),
    })
}

#[derive(Clone, Debug, Default)]
pub enum UnixAddr {
    #[default]
    Unbound,
    Abstract(Arc<[u8]>),
    Path(Arc<str>),
}

/// Synchronous transport operations for Unix-domain sockets.
/// Accept uses the inherent [`UnixTransport::accept`] method.
#[enum_dispatch]
pub trait UnixTransportOps: Configurable + Pollable + Send + Sync {
    fn bind(&self, slot: &BindEntry, local_endpoint: &UnixAddr) -> KResult;
    /// Starts listening and records the listener's own peer identity.
    ///
    /// Stream clients copy this snapshot when they connect, matching Linux
    /// `unix_listen` + `unix_stream_connect`.
    fn listen(&self, _backlog: usize, _credentials: &PeerCredentials) -> KResult {
        Err(KError::OperationNotSupported)
    }

    /// Connects to a bound peer.
    ///
    /// For streams, `credentials` is the connector's snapshot, which the
    /// accepted endpoint reports. Datagram transports ignore it because Linux
    /// does not establish peer credentials on `connect`.
    fn connect(
        &self,
        slot: &BindEntry,
        local_endpoint: &UnixAddr,
        options: ConnectOptions,
        _credentials: &PeerCredentials,
    ) -> KResult;

    fn send(&self, src: impl Read + IoBuf, options: SendOptions) -> KResult<usize>;
    fn recv(&self, dst: impl Write + IoBufMut, options: RecvOptions<'_>) -> KResult<usize>;

    fn shutdown(&self, _how: Shutdown) -> KResult {
        Ok(())
    }

    /// Byte counts for `SIOCOUTQ`/`SIOCINQ`, or `None` when this transport
    /// keeps its receive queue somewhere the socket layer cannot peek into.
    fn queue_lengths(&self) -> Option<QueueLengths> {
        None
    }

    /// Whether this transport has reached the listening state.
    fn is_listening(&self) -> bool {
        false
    }
}

#[enum_dispatch(Configurable, UnixTransportOps)]
pub enum UnixTransport {
    Stream(StreamTransport),
    Dgram(DgramTransport),
    Seqpacket(SeqpacketTransport),
}
impl UnixTransport {
    /// Accepts through static dispatch without boxing the future.
    ///
    /// Polling requires sleepable task context. Dropping a pending future
    /// cancels its wait; socket owner reservation remains the caller's job.
    pub async fn accept(&self, nonblocking: bool) -> KResult<(Self, UnixAddr)> {
        match self {
            Self::Stream(transport) => transport.accept(nonblocking).await,
            Self::Dgram(transport) => transport.accept(nonblocking).await,
            Self::Seqpacket(transport) => transport.accept(nonblocking).await,
        }
    }
}

impl Pollable for UnixTransport {
    fn poll(&self) -> IoEvents {
        match self {
            UnixTransport::Stream(stream) => stream.poll(),
            UnixTransport::Dgram(dgram) => dgram.poll(),
            UnixTransport::Seqpacket(seqpacket) => seqpacket.poll(),
        }
    }

    fn register(
        &self,
        context: &mut PollContext<'_>,
        events: IoEvents,
    ) -> Result<(), PollRegisterError> {
        match self {
            UnixTransport::Stream(stream) => stream.register(context, events),
            UnixTransport::Dgram(dgram) => dgram.register(context, events),
            UnixTransport::Seqpacket(seqpacket) => seqpacket.register(context, events),
        }
    }
}

pub struct BindEntry {
    pub(crate) stream: Arc<Mutex<Option<stream::Bind>>>,
    pub(crate) dgram: Arc<Mutex<Option<dgram::Bind>>>,
    pub(crate) seqpacket: Arc<Mutex<Option<seqpacket::Bind>>>,
}

impl Default for BindEntry {
    fn default() -> Self {
        Self {
            stream: Arc::new(Mutex::new(None)),
            dgram: Arc::new(Mutex::new(None)),
            seqpacket: Arc::new(Mutex::new(None)),
        }
    }
}

struct PathBindEntry {
    inode: WeakVfsInode,
    bind: Arc<BindEntry>,
}

struct AbstractBindEntry {
    bind: Arc<BindEntry>,
    owners: usize,
}

/// Owns a binding reservation until bind fails or the bound transport is dropped.
struct AbstractBinding {
    name: Arc<[u8]>,
    bind: Arc<BindEntry>,
}

impl AbstractBinding {
    fn reserve(name: Arc<[u8]>) -> Self {
        let mut bindings = ABSTRACT_BINDINGS.lock();
        let entry = bindings
            .entry(name.clone())
            .or_insert_with(|| AbstractBindEntry {
                bind: Arc::new(BindEntry::default()),
                owners: 0,
            });
        entry.owners += 1;
        Self {
            name,
            bind: entry.bind.clone(),
        }
    }
}

impl Drop for AbstractBinding {
    fn drop(&mut self) {
        let mut bindings = ABSTRACT_BINDINGS.lock();
        if let Some(entry) = bindings.get_mut(&self.name) {
            entry.owners -= 1;
            if entry.owners == 0 {
                bindings.remove(&self.name);
            }
        }
    }
}

lazy_static! {
    static ref ABSTRACT_BINDINGS: Mutex<HashMap<Arc<[u8]>, AbstractBindEntry>> =
        Mutex::new(HashMap::new());
    static ref PATH_BINDINGS: Mutex<HashMap<usize, PathBindEntry>> = Mutex::new(HashMap::new());
}

fn path_inode_key(loc: &Path) -> usize {
    loc.inode_key()
}

fn path_bind_matches(entry: &PathBindEntry, loc: &Path) -> bool {
    entry
        .inode
        .upgrade()
        .is_some_and(|inode| Arc::ptr_eq(&inode, &loc.inode()))
}

fn create_bind_location(
    root: &Path,
    pwd: &Path,
    path: &str,
    mode: NodePermission,
    cred: &kcred::Cred,
) -> KResult<Path> {
    let (parent, name) = Filename::new(path)
        .parent_at(root, pwd, LookupIntent::Open, cred)?
        .into_normal()?;
    parent
        .mknod(&name, NodeType::Socket, mode, DeviceId::default(), cred)
        .map_err(|error| {
            if error.canonicalize() == KError::AlreadyExists {
                KError::AddrInUse
            } else {
                error
            }
        })
}

pub(crate) fn lookup_bind_entry<R>(
    addr: &UnixAddr,
    cred: &kcred::Cred,
    f: impl FnOnce(&BindEntry) -> KResult<R>,
) -> KResult<R> {
    match addr {
        UnixAddr::Unbound => Err(KError::InvalidInput),
        UnixAddr::Abstract(name) => {
            let entry = ABSTRACT_BINDINGS
                .lock()
                .get(name)
                .map(|entry| entry.bind.clone())
                .ok_or(KError::ConnectionRefused)?;
            f(entry.as_ref())
        }
        UnixAddr::Path(path) => {
            let fs_struct = kprocess::current_fs_context();
            let fs = fs_struct.lock();
            let loc = Filename::new(path.as_ref()).lookup_at(
                fs.root(),
                fs.pwd(),
                LookupIntent::Open,
                LookupFlags::follow(),
                cred,
            )?;
            drop(fs);
            loc.permission(Permission::MAY_WRITE, cred)?;
            if loc.getattr()?.mode.node_type() != NodeType::Socket {
                return Err(KError::NotASocket);
            }
            let key = path_inode_key(&loc);
            let entry = {
                let mut bindings = PATH_BINDINGS.lock();
                let Some(entry) = bindings.get(&key) else {
                    return Err(KError::ConnectionRefused);
                };
                if !path_bind_matches(entry, &loc) {
                    bindings.remove(&key);
                    return Err(KError::ConnectionRefused);
                }
                entry.bind.clone()
            };
            f(entry.as_ref())
        }
    }
}
fn lookup_or_create_bind_entry(
    addr: &UnixAddr,
    mode: NodePermission,
    cred: &kcred::Cred,
    f: impl FnOnce(&BindEntry) -> KResult,
) -> KResult<Option<AbstractBinding>> {
    match addr {
        UnixAddr::Unbound => Err(KError::InvalidInput),
        UnixAddr::Abstract(name) => {
            let binding = AbstractBinding::reserve(name.clone());
            f(binding.bind.as_ref())?;
            Ok(Some(binding))
        }
        UnixAddr::Path(path) => {
            let fs_struct = kprocess::current_fs_context();
            let fs = fs_struct.lock();
            let loc = create_bind_location(fs.root(), fs.pwd(), path.as_ref(), mode, cred)?;
            drop(fs);
            if loc.getattr()?.mode.node_type() != NodeType::Socket {
                return Err(KError::NotASocket);
            }
            let key = path_inode_key(&loc);
            let entry = {
                let mut bindings = PATH_BINDINGS.lock();
                if bindings
                    .get(&key)
                    .is_some_and(|entry| !path_bind_matches(entry, &loc))
                {
                    bindings.remove(&key);
                }
                bindings
                    .entry(key)
                    .or_insert_with(|| PathBindEntry {
                        inode: loc.weak_inode(),
                        bind: Arc::new(BindEntry::default()),
                    })
                    .bind
                    .clone()
            };
            f(entry.as_ref())?;
            Ok(None)
        }
    }
}

/// The socket type requested at creation.
///
/// Linux keeps it in `sk_type` and reports it back from `SO_TYPE`, so it is
/// stored rather than derived from the transport: a socket may be created
/// before its transport kind is observable, and `unix_create` keeps
/// `SOCK_SEQPACKET` while rewriting only `SOCK_RAW` to `SOCK_DGRAM`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum UnixSocketType {
    Stream,
    Datagram,
    Seqpacket,
}

impl UnixSocketType {
    /// The `SOCK_*` value `SO_TYPE` reports for this type.
    pub const fn as_raw(self) -> i32 {
        match self {
            Self::Stream => linux_raw_sys::net::SOCK_STREAM as i32,
            Self::Datagram => linux_raw_sys::net::SOCK_DGRAM as i32,
            Self::Seqpacket => linux_raw_sys::net::SOCK_SEQPACKET as i32,
        }
    }
}

#[derive(Default)]
struct LocalBinding {
    addr: UnixAddr,
    // Accepted sockets keep the address without owning its registration.
    _abstract_binding: Option<AbstractBinding>,
}

pub struct UnixDomainSocket {
    // Drop the transport (and clear its slot) before releasing the registration.
    transport: UnixTransport,
    socket_type: UnixSocketType,
    local_binding: Mutex<LocalBinding>,
    peer_endpoint: Mutex<UnixAddr>,
}
impl UnixDomainSocket {
    fn accept_with_storage(
        &self,
        options: AcceptOptions,
        prepare: impl FnOnce() -> KResult<KBox<core::mem::MaybeUninit<Self>>>,
    ) -> KResult<Socket> {
        let storage = prepare()?;
        let (transport, peer_endpoint) =
            block_on_interruptible(self.transport.accept(options.nonblocking))??;
        Ok(Socket::Unix(KBox::write(
            storage,
            Self {
                transport,
                // The accepted socket keeps the listener's type, as Linux keeps the
                // listening socket's `sk_type` for the connection it hands back.
                socket_type: self.socket_type,
                local_binding: Mutex::new(LocalBinding {
                    addr: self.local_binding.lock().addr.clone(),
                    ..LocalBinding::default()
                }),
                peer_endpoint: Mutex::new(peer_endpoint),
            },
        )))
    }

    pub fn new(transport: impl Into<UnixTransport>, socket_type: UnixSocketType) -> Self {
        Self {
            transport: transport.into(),
            socket_type,
            local_binding: Mutex::new(LocalBinding::default()),
            peer_endpoint: Mutex::new(UnixAddr::Unbound),
        }
    }

    /// Whether this socket uses the datagram transport.
    ///
    /// Seqpacket sockets now have their own connection-oriented transport, so
    /// only genuine `SOCK_DGRAM` sockets answer `true`.
    pub fn is_datagram(&self) -> bool {
        matches!(&self.transport, UnixTransport::Dgram(_))
    }

    /// Whether a send should capture the caller's credentials.
    ///
    /// Linux datagram sends always capture credentials so a receiver with
    /// `SO_PASSCRED` can observe the real sender. Stream sends capture when the
    /// sender or the connected receiver enabled `SO_PASSCRED`; Linux
    /// `unix_passcred_enabled` tests both sockets (`other` is the receiving
    /// peer). Ordinary stream traffic with neither option enabled adds no
    /// per-message credential barrier.
    pub fn should_attach_sender_credentials(&self) -> bool {
        match &self.transport {
            // Linux sends seqpacket messages through the datagram path, which
            // always captures the sender so a receiver with `SO_PASSCRED` can
            // observe it.
            UnixTransport::Dgram(_) | UnixTransport::Seqpacket(_) => true,
            UnixTransport::Stream(stream) => {
                stream.pass_credentials() || stream.peer_pass_credentials()
            }
        }
    }

    /// Binds this socket using an explicit credential for pathname access.
    ///
    /// Kernel callers use this entry because they do not have a current user
    /// task. User-facing bind enters through [`SocketOps::bind`], which takes
    /// one snapshot of the current task credential before calling this method.
    /// An unbound address requests automatic allocation in the abstract namespace.
    /// Repeating that request preserves an existing local binding.
    ///
    /// # Execution context
    ///
    /// Requires sleepable task context with IRQs and preemption enabled and no
    /// spinlocks held. Automatic allocation may read entropy sources and yield
    /// while retrying occupied names. Pathname binding also requires a current
    /// filesystem context; abstract binding can run on a kernel task.
    pub fn bind_with_cred(
        &self,
        local_endpoint: SocketAddrEx,
        cred: &kcred::Cred,
        mode: NodePermission,
    ) -> KResult {
        let mut local_endpoint = local_endpoint.into_unix()?;
        let mut local_binding = self.local_binding.lock();
        let is_autobind = matches!(&local_endpoint, UnixAddr::Unbound);
        if !matches!(&local_binding.addr, UnixAddr::Unbound) {
            return if is_autobind {
                Ok(())
            } else {
                Err(KError::InvalidInput)
            };
        }
        let binding = if is_autobind {
            let (addr, binding) = self.autobind()?;
            local_endpoint = addr;
            Some(binding)
        } else {
            lookup_or_create_bind_entry(&local_endpoint, mode, cred, |slot| {
                self.transport.bind(slot, &local_endpoint)
            })?
        };
        *local_binding = LocalBinding {
            addr: local_endpoint,
            _abstract_binding: binding,
        };
        Ok(())
    }

    fn autobind(&self) -> KResult<(UnixAddr, AbstractBinding)> {
        const NAME_COUNT: u32 = 1 << 20;
        const HEX: &[u8; 16] = b"0123456789abcdef";

        let mut seed = [0; 4];
        entropy::fill_random(&mut seed);
        let start = u32::from_ne_bytes(seed);
        for offset in 0..NAME_COUNT {
            let mut number = start.wrapping_add(offset) & (NAME_COUNT - 1);
            let mut name = [0; 5];
            for byte in name.iter_mut().rev() {
                *byte = HEX[(number & 0xf) as usize];
                number >>= 4;
            }
            let binding = AbstractBinding::reserve(Arc::from(name));
            let addr = UnixAddr::Abstract(binding.name.clone());
            match self.transport.bind(&binding.bind, &addr) {
                Ok(()) => return Ok((addr, binding)),
                Err(error) if error.canonicalize() == KError::AddrInUse => {}
                Err(error) => return Err(error),
            }
            drop(binding);
            ktask::yield_now();
        }
        Err(KError::StorageFull)
    }

    /// Starts listening with an explicit snapshot of this listener's identity.
    ///
    /// Kernel callers without a current user thread use this inherent entry on
    /// the concrete Unix socket before erasing it into a generic [`Socket`]; it
    /// neither reads nor requires a user-task credential. [`SocketOps::listen`]
    /// remains the user-facing path and snapshots the current user thread.
    /// Datagram transports ignore the snapshot because `listen` does not
    /// establish a peer there.
    pub fn listen_with_credentials(
        &self,
        backlog: usize,
        credentials: &PeerCredentials,
    ) -> KResult {
        self.transport.listen(backlog, credentials)
    }

    /// Connects using explicit credentials for pathname lookup and, for a
    /// stream, an explicit connector identity for the accepted endpoint.
    ///
    /// User-facing [`SocketOps::connect`] snapshots both values from the current
    /// task. Kernel callers without a user thread supply them here on the
    /// concrete Unix socket before erasing it into a generic [`Socket`].
    /// Datagram transports ignore `peer_credentials` because `connect`
    /// establishes no peer identity.
    pub fn connect_with_credentials(
        &self,
        remote_addr: SocketAddrEx,
        options: ConnectOptions,
        cred: &kcred::Cred,
        peer_credentials: &PeerCredentials,
    ) -> KResult {
        self.connect_unix_with_credentials(
            remote_addr.into_unix()?,
            options,
            cred,
            peer_credentials,
        )
    }

    fn connect_unix_with_credentials(
        &self,
        remote_addr: UnixAddr,
        options: ConnectOptions,
        cred: &kcred::Cred,
        peer_credentials: &PeerCredentials,
    ) -> KResult {
        let local_endpoint = self.local_binding.lock().addr.clone();
        lookup_bind_entry(&remote_addr, cred, |slot| {
            self.transport
                .connect(slot, &local_endpoint, options, peer_credentials)
        })?;
        *self.peer_endpoint.lock() = remote_addr;
        Ok(())
    }
}
impl Configurable for UnixDomainSocket {
    fn get_option_inner(&self, opt: &mut GetSocketOption) -> KResult<OptionHandled> {
        self.transport.get_option_inner(opt)
    }

    fn set_option_inner(&self, opt: SetSocketOption) -> KResult<OptionHandled> {
        self.transport.set_option_inner(opt)
    }
}
impl SocketOps for UnixDomainSocket {
    /// Returns the type the socket was created with, the way Linux reports
    /// `sk_type` back from `SO_TYPE`. Deriving it from the transport would
    /// report `SOCK_DGRAM` for `SOCK_SEQPACKET` pairs.
    fn socket_type(&self) -> i32 {
        self.socket_type.as_raw()
    }

    fn socket_family(&self) -> i32 {
        linux_raw_sys::net::AF_UNIX as i32
    }

    /// `unix_create` never records a protocol, so Linux leaves `sk_protocol` 0.
    fn socket_protocol(&self) -> i32 {
        0
    }

    fn bind(&self, local_endpoint: SocketAddrEx) -> KResult {
        let cred = kprocess::current_cred();
        let mode = NodePermission::from_bits_truncate(0o777 & !(kprocess::current_umask() as u16));
        self.bind_with_cred(local_endpoint, &cred, mode)
    }

    fn connect(&self, remote_addr: SocketAddrEx, options: ConnectOptions) -> KResult {
        let remote_addr = remote_addr.into_unix()?;
        let cred = kprocess::current_cred();
        // Linux initializes peer credentials only while establishing a
        // connection; datagram `connect` leaves `sk_peer_pid` unset.
        let peer_credentials = match &self.transport {
            UnixTransport::Stream(_) | UnixTransport::Seqpacket(_) => current_peer_credentials(),
            UnixTransport::Dgram(_) => PeerCredentials::none(),
        };
        self.connect_unix_with_credentials(remote_addr, options, &cred, &peer_credentials)
    }

    fn listen(&self, backlog: usize) -> KResult {
        // Only a connection-oriented listener records an identity for future
        // clients.
        let peer_credentials = match &self.transport {
            UnixTransport::Stream(_) | UnixTransport::Seqpacket(_) => current_peer_credentials(),
            UnixTransport::Dgram(_) => PeerCredentials::none(),
        };
        self.listen_with_credentials(backlog, &peer_credentials)
    }

    fn accept(&self, options: AcceptOptions) -> KResult<Socket> {
        self.accept_with_storage(options, KBox::<Self>::new_uninit)
    }

    fn send(&self, src: impl Read + IoBuf, options: SendOptions) -> KResult<usize> {
        self.transport.send(src, options)
    }

    fn recv(&self, dst: impl Write + IoBufMut, options: RecvOptions<'_>) -> KResult<usize> {
        self.transport.recv(dst, options)
    }

    fn local_addr(&self) -> KResult<SocketAddrEx> {
        Ok(SocketAddrEx::Unix(self.local_binding.lock().addr.clone()))
    }

    fn peer_addr(&self) -> KResult<SocketAddrEx> {
        Ok(SocketAddrEx::Unix(self.peer_endpoint.lock().clone()))
    }

    fn shutdown(&self, how: Shutdown) -> KResult {
        self.transport.shutdown(how)
    }

    fn queue_lengths(&self) -> KResult<Option<QueueLengths>> {
        Ok(self.transport.queue_lengths())
    }

    fn is_listening(&self) -> bool {
        self.transport.is_listening()
    }
}

impl Pollable for UnixDomainSocket {
    fn poll(&self) -> IoEvents {
        self.transport.poll()
    }

    fn register(
        &self,
        context: &mut PollContext<'_>,
        events: IoEvents,
    ) -> Result<(), PollRegisterError> {
        self.transport.register(context, events)
    }
}

unittest::path_mod_test!(tests, "../ut-tests/unix/tests.rs");
