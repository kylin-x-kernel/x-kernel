//! TCP listen table and backlog management.
use alloc::{boxed::Box, collections::VecDeque, sync::Arc, vec};
use core::ops::DerefMut;

use kerrno::{KError, KResult};
use ksync::Mutex;
use smoltcp::{
    iface::{SocketHandle, SocketSet},
    socket::tcp::{self, SocketBuffer, State},
    wire::{IpEndpoint, IpListenEndpoint},
};

use crate::{
    SOCKET_SET,
    consts::{LISTEN_QUEUE_SIZE, TCP_RX_BUF_LEN, TCP_TX_BUF_LEN},
};

const PORT_NUM: usize = 65536;

struct ListenTableEntry {
    listen_endpoint: IpListenEndpoint,
    syn_queue: VecDeque<SocketHandle>,
}

impl ListenTableEntry {
    /// Create a new listen table entry for the given endpoint.
    pub fn new(listen_endpoint: IpListenEndpoint) -> Self {
        Self {
            listen_endpoint,
            syn_queue: VecDeque::with_capacity(LISTEN_QUEUE_SIZE),
        }
    }
}

impl Drop for ListenTableEntry {
    fn drop(&mut self) {
        for &dispatch_irq in &self.syn_queue {
            SOCKET_SET.remove(dispatch_irq);
        }
    }
}

pub struct ListenTable {
    tcp: TcpListenTable,
}

type TcpListenTable = Box<[Arc<Mutex<Option<Box<ListenTableEntry>>>>]>;

impl ListenTable {
    /// Create an empty listen table.
    pub fn new() -> Self {
        let tcp = unsafe {
            let mut buf = Box::new_uninit_slice(PORT_NUM);
            for i in 0..PORT_NUM {
                buf[i].write(Arc::default());
            }
            buf.assume_init()
        };
        Self { tcp }
    }

    pub fn can_listen(&self, port: u16) -> bool {
        self.tcp[port as usize].lock().is_none()
    }

    pub fn listen(&self, listen_endpoint: IpListenEndpoint) -> KResult {
        let port = listen_endpoint.port;
        assert_ne!(port, 0);
        let mut entry = self.tcp[port as usize].lock();
        if entry.is_none() {
            *entry = Some(Box::new(ListenTableEntry::new(listen_endpoint)));
            Ok(())
        } else {
            warn!("socket already listening on port {port}");
            Err(KError::AddrInUse)
        }
    }

    pub fn unlisten(&self, port: u16) {
        debug!("TCP socket unlisten on {}", port);
        *self.tcp[port as usize].lock() = None;
    }

    fn listen_entry(&self, port: u16) -> Arc<Mutex<Option<Box<ListenTableEntry>>>> {
        self.tcp[port as usize].clone()
    }

    pub fn can_accept(&self, port: u16) -> KResult<bool> {
        if let Some(entry) = self.listen_entry(port).lock().as_ref() {
            Ok(entry
                .syn_queue
                .iter()
                .any(|&dispatch_irq| is_connected(dispatch_irq)))
        } else {
            warn!("accept before listen");
            Err(KError::InvalidInput)
        }
    }

    pub fn accept(&self, port: u16) -> KResult<SocketHandle> {
        let entry = self.listen_entry(port);
        let mut table = entry.lock();
        let Some(entry) = table.deref_mut() else {
            warn!("accept before listen");
            return Err(KError::InvalidInput);
        };

        let syn_queue: &mut VecDeque<SocketHandle> = &mut entry.syn_queue;
        let idx = syn_queue
            .iter()
            .enumerate()
            .find_map(|(idx, &dispatch_irq)| is_connected(dispatch_irq).then_some(idx))
            .ok_or(KError::WouldBlock)?; // wait for connection
        if idx > 0 {
            warn!(
                "slow SYN queue enumeration: index = {}, len = {}!",
                idx,
                syn_queue.len()
            );
        }
        let dispatch_irq = syn_queue.swap_remove_front(idx).unwrap();
        // If the connection is reset, return ConnectionReset error
        // Otherwise, return the dispatch_irq and the address tuple
        if is_closed(dispatch_irq) {
            warn!("accept failed: connection reset");
            Err(KError::ConnectionReset)
        } else {
            Ok(dispatch_irq)
        }
    }

    pub fn incoming_tcp_packet(
        &self,
        src: IpEndpoint,
        dst: IpEndpoint,
        sockets: &mut SocketSet<'_>,
    ) {
        if let Some(entry) = self.listen_entry(dst.port).lock().deref_mut() {
            // TODO(mivik): accept address check
            if entry.syn_queue.len() >= LISTEN_QUEUE_SIZE {
                // SYN queue is full, drop the packet
                warn!("SYN queue overflow!");
                return;
            }

            let mut socket = smoltcp::socket::tcp::Socket::new(
                SocketBuffer::new(vec![0; TCP_RX_BUF_LEN]),
                SocketBuffer::new(vec![0; TCP_TX_BUF_LEN]),
            );
            if let Err(err) = socket.listen(IpListenEndpoint {
                addr: None,
                port: dst.port,
            }) {
                warn!("Failed to listen on {}: {:?}", entry.listen_endpoint, err);
                return;
            }
            let dispatch_irq = sockets.add(socket);
            debug!(
                "TCP socket {}: prepare for connection {} -> {}",
                dispatch_irq, src, entry.listen_endpoint
            );
            entry.syn_queue.push_back(dispatch_irq);
        }
    }
}

fn is_connected(dispatch_irq: SocketHandle) -> bool {
    SOCKET_SET.with_socket::<tcp::Socket, _, _>(dispatch_irq, |socket| {
        !matches!(socket.state(), State::Listen | State::SynReceived)
    })
}

fn is_closed(dispatch_irq: SocketHandle) -> bool {
    SOCKET_SET.with_socket::<tcp::Socket, _, _>(dispatch_irq, |socket| {
        matches!(socket.state(), State::Closed)
    })
}
