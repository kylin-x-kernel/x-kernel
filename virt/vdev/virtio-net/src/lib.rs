// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Minimal virtio-mmio network backend with a smoke-test data path.

#![no_std]

extern crate alloc;

use alloc::{sync::Arc, vec::Vec};
use core::net::{IpAddr, Ipv4Addr, SocketAddr};

use vdev_core::{GuestDma, IrqSender, MmioDevice};

pub const VIRTIO_NET_BASE: u64 = 0x0a00_0000;
pub const VIRTIO_NET_SIZE: u64 = 0x1000;
pub const VIRTIO_NET_IRQ: u32 = 48;

const MAGIC: u64 = 0x000;
const VERSION: u64 = 0x004;
const DEVICE_ID: u64 = 0x008;
const VENDOR_ID: u64 = 0x00c;
const DEVICE_FEATURES: u64 = 0x010;
const DEVICE_FEATURES_SEL: u64 = 0x014;
const DRIVER_FEATURES: u64 = 0x020;
const DRIVER_FEATURES_SEL: u64 = 0x024;
const QUEUE_SEL: u64 = 0x030;
const QUEUE_NUM_MAX: u64 = 0x034;
const QUEUE_NUM: u64 = 0x038;
const QUEUE_READY: u64 = 0x044;
const QUEUE_NOTIFY: u64 = 0x050;
const INTERRUPT_STATUS: u64 = 0x060;
const INTERRUPT_ACK: u64 = 0x064;
const STATUS: u64 = 0x070;
const QUEUE_DESC_LOW: u64 = 0x080;
const QUEUE_DESC_HIGH: u64 = 0x084;
const QUEUE_DRIVER_LOW: u64 = 0x090;
const QUEUE_DRIVER_HIGH: u64 = 0x094;
const QUEUE_DEVICE_LOW: u64 = 0x0a0;
const QUEUE_DEVICE_HIGH: u64 = 0x0a4;
const CONFIG_GENERATION: u64 = 0x0fc;
const CONFIG_MAC: u64 = 0x100;
const CONFIG_MAC_END: u64 = CONFIG_MAC + 5;

const VIRTIO_MAGIC: u32 = 0x7472_6976;
const VIRTIO_VERSION_2: u32 = 2;
const VIRTIO_DEVICE_NET: u32 = 1;
const X_KERNEL_VENDOR_ID: u32 = 0x584b_564d;
const VIRTIO_F_VERSION_1: u64 = 1 << 32;
const VIRTIO_NET_F_MAC: u64 = 1 << 5;

const ISR_USED_RING: u32 = 1 << 0;
const MAX_QUEUES: usize = 2;
const QUEUE_SIZE: u32 = 256;
const DESC_SIZE: u64 = 16;
const DESC_F_WRITE: u16 = 2;
const HOST_EGRESS_IFINDEX: i32 = 2;
const MAX_FRAME_LEN: usize = 1514;
const GUEST_MAC: [u8; 6] = [0x02, 0x58, 0x4b, 0x09, 0x0a, 0x0b];
const HOST_MAC: [u8; 6] = [0x02, 0x58, 0x4b, 0, 0, 1];
const GUEST_IP: [u8; 4] = [10, 0, 2, 16];
const HOST_IP: [u8; 4] = [10, 0, 2, 2];
const GUEST_UDP_PORT: u16 = 5555;
const HOST_UDP_PORT: u16 = 5555;

#[derive(Clone, Copy, Default)]
struct QueueState {
    size: u32,
    ready: bool,
    desc: u64,
    driver: u64,
    device: u64,
    notify_count: u64,
}

#[derive(Clone, Copy)]
struct Desc {
    addr: u64,
    len: u32,
    flags: u16,
}

pub struct VirtioNet {
    vm_id: u32,
    target_vcpu: u32,
    irq: u32,
    irq_sender: Arc<dyn IrqSender>,
    dma: Arc<dyn GuestDma>,
    device_features_sel: u32,
    driver_features_sel: u32,
    driver_features: u64,
    queue_sel: u32,
    queues: [QueueState; MAX_QUEUES],
    last_avail_idx: [u16; MAX_QUEUES],
    status: u32,
    interrupt_status: u32,
    mac: [u8; 6],
    udp_relay: Option<knet::UdpDatagramRelay>,
    udp_ingress: Option<knet::UdpDatagramRelay>,
    pending_rx_payload: Vec<u8>,
    did_send_fake_rx: bool,
}

impl VirtioNet {
    pub fn new(vm_id: u32, irq_sender: Arc<dyn IrqSender>, dma: Arc<dyn GuestDma>) -> Self {
        Self {
            vm_id,
            target_vcpu: 0,
            irq: VIRTIO_NET_IRQ,
            irq_sender,
            dma,
            device_features_sel: 0,
            driver_features_sel: 0,
            driver_features: 0,
            queue_sel: 0,
            queues: [QueueState::default(); MAX_QUEUES],
            last_avail_idx: [0; MAX_QUEUES],
            status: 0,
            interrupt_status: 0,
            mac: [0x02, 0x58, 0x4b, 0x00, 0x00, vm_id as u8],
            udp_relay: None,
            udp_ingress: None,
            pending_rx_payload: Vec::new(),
            did_send_fake_rx: false,
        }
    }

    fn selected_queue(&self) -> Option<&QueueState> {
        self.queues.get(self.queue_sel as usize)
    }

    fn selected_queue_mut(&mut self) -> Option<&mut QueueState> {
        self.queues.get_mut(self.queue_sel as usize)
    }

    fn device_features(&self) -> u64 {
        VIRTIO_F_VERSION_1 | VIRTIO_NET_F_MAC
    }

    fn notify_queue(&mut self, queue: u32) {
        let Some(q) = self.queues.get_mut(queue as usize) else {
            return;
        };
        q.notify_count = q.notify_count.wrapping_add(1);
        match queue {
            0 => self.process_rx_queue(),
            1 => {
                self.process_tx_queue();
                self.process_rx_queue();
            }
            _ => {}
        }
        self.interrupt_status |= ISR_USED_RING;
        self.irq_sender.inject(self.target_vcpu, self.irq);
        self.trace_notify(queue);
    }

    fn process_tx_queue(&mut self) {
        const QUEUE_TX: usize = 1;
        while let Some(head) = self.pop_avail(QUEUE_TX) {
            let Some(desc) = self.read_desc(QUEUE_TX, head) else {
                break;
            };
            let len = (desc.len as usize).min(MAX_FRAME_LEN);
            let mut buf = alloc::vec![0; len];
            if self.dma.read(desc.addr, &mut buf).is_ok() {
                log::info!(
                    "[virtio-net] vm{} TX desc={} len={} first={:02x?}",
                    self.vm_id,
                    head,
                    desc.len,
                    &buf[..len.min(16)],
                );
                match knet::send_link_frame(HOST_EGRESS_IFINDEX, &buf) {
                    Ok(sent) => log::info!(
                        "[virtio-net] vm{} forwarded {} bytes to ifindex {}",
                        self.vm_id,
                        sent,
                        HOST_EGRESS_IFINDEX,
                    ),
                    Err(err) => log::warn!(
                        "[virtio-net] vm{} failed to forward TX frame to ifindex {}: {:?}",
                        self.vm_id,
                        HOST_EGRESS_IFINDEX,
                        err,
                    ),
                }
                if let Some((dst, payload)) = udp_payload_from_frame(&buf) {
                    let vm_id = self.vm_id;
                    match self.with_udp_relay(|relay| relay.send_to(dst, payload)) {
                        Some(Ok(sent)) => log::info!(
                            "[virtio-net] vm{} relayed {} UDP payload bytes to {}",
                            vm_id,
                            sent,
                            dst,
                        ),
                        Some(Err(err)) => log::warn!(
                            "[virtio-net] vm{} failed to relay UDP payload to {}: {:?}",
                            vm_id,
                            dst,
                            err,
                        ),
                        None => {}
                    }
                }
            }
            self.push_used(QUEUE_TX, head as u32, desc.len);
        }
    }

    fn with_udp_relay<R>(&mut self, f: impl FnOnce(&knet::UdpDatagramRelay) -> R) -> Option<R> {
        if self.udp_relay.is_none() {
            match knet::UdpDatagramRelay::new() {
                Ok(relay) => self.udp_relay = Some(relay),
                Err(err) => {
                    log::warn!(
                        "[virtio-net] vm{} failed to create UDP relay: {:?}",
                        self.vm_id,
                        err,
                    );
                    return None;
                }
            }
        }
        Some(f(self.udp_relay.as_ref().expect("UDP relay initialized")))
    }

    fn with_udp_ingress<R>(&mut self, f: impl FnOnce(&knet::UdpDatagramRelay) -> R) -> Option<R> {
        if self.udp_ingress.is_none() {
            match knet::UdpDatagramRelay::new_with_port(GUEST_UDP_PORT) {
                Ok(relay) => self.udp_ingress = Some(relay),
                Err(err) => {
                    log::warn!(
                        "[virtio-net] vm{} failed to create UDP ingress on port {}: {:?}",
                        self.vm_id,
                        GUEST_UDP_PORT,
                        err,
                    );
                    return None;
                }
            }
        }
        Some(f(self
            .udp_ingress
            .as_ref()
            .expect("UDP ingress initialized")))
    }

    fn process_rx_queue(&mut self) {
        const QUEUE_RX: usize = 0;
        while let Some(head) = self.pop_avail(QUEUE_RX) {
            let Some(desc) = self.read_desc(QUEUE_RX, head) else {
                break;
            };
            if desc.flags & DESC_F_WRITE == 0 {
                log::warn!(
                    "[virtio-net] vm{} RX desc={} missing WRITE flags={:#x}",
                    self.vm_id,
                    head,
                    desc.flags,
                );
                self.push_used(QUEUE_RX, head as u32, 0);
                continue;
            }
            self.poll_udp_relay_rx();
            let frame = if self.pending_rx_payload.is_empty() {
                if self.did_send_fake_rx {
                    self.last_avail_idx[QUEUE_RX] = self.last_avail_idx[QUEUE_RX].wrapping_sub(1);
                    break;
                }
                self.did_send_fake_rx = true;
                fake_rx_frame().to_vec()
            } else {
                let frame = udp_rx_frame(&self.pending_rx_payload);
                self.pending_rx_payload.clear();
                frame
            };
            let len = frame.len().min(desc.len as usize);
            if self.dma.write(desc.addr, &frame[..len]).is_ok() {
                log::info!(
                    "[virtio-net] vm{} RX desc={} len={} wrote={}",
                    self.vm_id,
                    head,
                    desc.len,
                    len,
                );
            }
            self.push_used(QUEUE_RX, head as u32, len as u32);
        }
    }

    fn poll_udp_relay_rx(&mut self) {
        if !self.pending_rx_payload.is_empty() {
            return;
        }
        let mut buf = [0u8; 256];
        let result = self
            .with_udp_ingress(|relay| relay.try_recv(&mut buf))
            .or_else(|| self.with_udp_relay(|relay| relay.try_recv(&mut buf)));
        match result {
            Some(Ok(Some((len, src)))) => {
                log::info!(
                    "[virtio-net] vm{} received {} UDP payload bytes from {}",
                    self.vm_id,
                    len,
                    src,
                );
                self.pending_rx_payload.extend_from_slice(&buf[..len]);
            }
            Some(Ok(None)) | None => {}
            Some(Err(err)) => log::warn!(
                "[virtio-net] vm{} failed to poll UDP relay RX: {:?}",
                self.vm_id,
                err,
            ),
        }
    }

    fn pop_avail(&mut self, queue: usize) -> Option<u16> {
        let q = self.queues.get(queue)?;
        if !q.ready || q.size == 0 || q.driver == 0 {
            return None;
        }
        let avail_idx = self.dma.read_u16(q.driver + 2).ok()?;
        let last = self.last_avail_idx[queue];
        log::trace!(
            "[virtio-net] vm{} queue{} avail_idx={} last={} ready={} size={} avail={:#x}",
            self.vm_id,
            queue,
            avail_idx,
            last,
            q.ready,
            q.size,
            q.driver,
        );
        if last == avail_idx {
            return None;
        }
        let ring = q.driver + 4 + ((last as u32 % q.size) as u64) * 2;
        let head = self.dma.read_u16(ring).ok()?;
        self.last_avail_idx[queue] = last.wrapping_add(1);
        Some(head)
    }

    fn read_desc(&self, queue: usize, index: u16) -> Option<Desc> {
        let q = self.queues.get(queue)?;
        if index as u32 >= q.size || q.desc == 0 {
            return None;
        }
        let base = q.desc + index as u64 * DESC_SIZE;
        Some(Desc {
            addr: self.dma.read_u64(base).ok()?,
            len: self.dma.read_u32(base + 8).ok()?,
            flags: self.dma.read_u16(base + 12).ok()?,
        })
    }

    fn push_used(&self, queue: usize, id: u32, len: u32) {
        let Some(q) = self.queues.get(queue) else {
            return;
        };
        if q.size == 0 || q.device == 0 {
            return;
        }
        let used_idx = self.dma.read_u16(q.device + 2).unwrap_or(0);
        let elem = q.device + 4 + ((used_idx as u32 % q.size) as u64) * 8;
        let _ = self.dma.write_u32(elem, id);
        let _ = self.dma.write_u32(elem + 4, len);
        let _ = self.dma.write_u16(q.device + 2, used_idx.wrapping_add(1));
    }

    fn trace_notify(&self, queue: u32) {
        let Some(q) = self.queues.get(queue as usize) else {
            return;
        };
        log::trace!(
            "[virtio-net] vm{} notify queue{} ready={} size={} desc={:#x} avail={:#x} used={:#x}",
            self.vm_id,
            queue,
            q.ready,
            q.size,
            q.desc,
            q.driver,
            q.device,
        );
    }

    fn reset(&mut self) {
        self.driver_features = 0;
        self.queue_sel = 0;
        self.queues = [QueueState::default(); MAX_QUEUES];
        self.last_avail_idx = [0; MAX_QUEUES];
        self.status = 0;
        self.interrupt_status = 0;
        self.pending_rx_payload.clear();
        self.did_send_fake_rx = false;
    }
}

fn fake_rx_frame() -> [u8; 60] {
    let mut frame = [0u8; 60];
    frame[0..6].copy_from_slice(&[0xff; 6]);
    frame[6..12].copy_from_slice(&[0x02, 0x58, 0x4b, 0, 0, 1]);
    frame[12] = 0x08;
    frame[13] = 0x00;
    frame[14..30].copy_from_slice(b"x-kernel-vnet-rx");
    frame
}

fn udp_rx_frame(payload: &[u8]) -> Vec<u8> {
    let udp_len = 8 + payload.len();
    let ip_len = 20 + udp_len;
    let frame_len = 14 + ip_len;
    let mut frame = alloc::vec![0; frame_len];

    frame[0..6].copy_from_slice(&GUEST_MAC);
    frame[6..12].copy_from_slice(&HOST_MAC);
    frame[12..14].copy_from_slice(&[0x08, 0x00]);

    let ip = 14;
    frame[ip] = 0x45;
    frame[ip + 2..ip + 4].copy_from_slice(&(ip_len as u16).to_be_bytes());
    frame[ip + 4..ip + 6].copy_from_slice(&0x584c_u16.to_be_bytes());
    frame[ip + 6..ip + 8].copy_from_slice(&0x4000_u16.to_be_bytes());
    frame[ip + 8] = 64;
    frame[ip + 9] = 17;
    frame[ip + 12..ip + 16].copy_from_slice(&HOST_IP);
    frame[ip + 16..ip + 20].copy_from_slice(&GUEST_IP);
    let ip_checksum = checksum16(&frame[ip..ip + 20]);
    frame[ip + 10..ip + 12].copy_from_slice(&ip_checksum.to_be_bytes());

    let udp = ip + 20;
    frame[udp..udp + 2].copy_from_slice(&HOST_UDP_PORT.to_be_bytes());
    frame[udp + 2..udp + 4].copy_from_slice(&GUEST_UDP_PORT.to_be_bytes());
    frame[udp + 4..udp + 6].copy_from_slice(&(udp_len as u16).to_be_bytes());
    frame[udp + 8..udp + 8 + payload.len()].copy_from_slice(payload);
    let udp_checksum = udp_checksum(&frame[ip..ip + 20], &frame[udp..udp + udp_len]);
    frame[udp + 6..udp + 8].copy_from_slice(&udp_checksum.to_be_bytes());

    frame
}

fn checksum16(buf: &[u8]) -> u16 {
    let mut sum = 0u32;
    let mut chunks = buf.chunks_exact(2);
    for chunk in &mut chunks {
        sum += u16::from_be_bytes([chunk[0], chunk[1]]) as u32;
    }
    if let Some(&byte) = chunks.remainder().first() {
        sum += (byte as u32) << 8;
    }
    while sum >> 16 != 0 {
        sum = (sum & 0xffff) + (sum >> 16);
    }
    !(sum as u16)
}

fn udp_checksum(ip: &[u8], udp: &[u8]) -> u16 {
    let mut pseudo = [0u8; 12];
    pseudo[0..4].copy_from_slice(&ip[12..16]);
    pseudo[4..8].copy_from_slice(&ip[16..20]);
    pseudo[9] = 17;
    pseudo[10..12].copy_from_slice(&(udp.len() as u16).to_be_bytes());

    let mut sum = checksum_accumulate(0, &pseudo);
    sum = checksum_accumulate(sum, udp);
    while sum >> 16 != 0 {
        sum = (sum & 0xffff) + (sum >> 16);
    }
    !(sum as u16)
}

fn checksum_accumulate(mut sum: u32, buf: &[u8]) -> u32 {
    let mut chunks = buf.chunks_exact(2);
    for chunk in &mut chunks {
        sum += u16::from_be_bytes([chunk[0], chunk[1]]) as u32;
    }
    if let Some(&byte) = chunks.remainder().first() {
        sum += (byte as u32) << 8;
    }
    sum
}

fn udp_payload_from_frame(frame: &[u8]) -> Option<(SocketAddr, &[u8])> {
    if frame.len() < 14 || frame[12] != 0x08 || frame[13] != 0x00 {
        return None;
    }

    let ip = &frame[14..];
    if ip.len() < 20 || ip[0] >> 4 != 4 || ip[9] != 17 {
        return None;
    }
    let ip_header_len = ((ip[0] & 0x0f) as usize) * 4;
    if ip_header_len < 20 || ip.len() < ip_header_len {
        return None;
    }
    let ip_total_len = u16::from_be_bytes([ip[2], ip[3]]) as usize;
    if ip_total_len < ip_header_len || ip.len() < ip_total_len {
        return None;
    }

    let udp = &ip[ip_header_len..ip_total_len];
    if udp.len() < 8 {
        return None;
    }
    let udp_len = u16::from_be_bytes([udp[4], udp[5]]) as usize;
    if udp_len < 8 || udp.len() < udp_len {
        return None;
    }

    let dst = SocketAddr::new(
        IpAddr::V4(Ipv4Addr::new(ip[16], ip[17], ip[18], ip[19])),
        u16::from_be_bytes([udp[2], udp[3]]),
    );
    Some((dst, &udp[8..udp_len]))
}

impl MmioDevice for VirtioNet {
    fn name(&self) -> &str {
        "virtio-net"
    }

    fn mmio_range(&self) -> (u64, u64) {
        (VIRTIO_NET_BASE, VIRTIO_NET_SIZE)
    }

    fn read(&self, offset: u64, _size: u8) -> u64 {
        match offset {
            MAGIC => VIRTIO_MAGIC as u64,
            VERSION => VIRTIO_VERSION_2 as u64,
            DEVICE_ID => VIRTIO_DEVICE_NET as u64,
            VENDOR_ID => X_KERNEL_VENDOR_ID as u64,
            DEVICE_FEATURES => {
                ((self.device_features() >> (self.device_features_sel * 32)) as u32) as u64
            }
            DEVICE_FEATURES_SEL => self.device_features_sel as u64,
            DRIVER_FEATURES_SEL => self.driver_features_sel as u64,
            QUEUE_SEL => self.queue_sel as u64,
            QUEUE_NUM_MAX if (self.queue_sel as usize) < MAX_QUEUES => QUEUE_SIZE as u64,
            QUEUE_NUM_MAX => 0,
            QUEUE_NUM => self.selected_queue().map_or(0, |q| q.size as u64),
            QUEUE_READY => self.selected_queue().map_or(0, |q| q.ready as u64),
            INTERRUPT_STATUS => self.interrupt_status as u64,
            STATUS => self.status as u64,
            QUEUE_DESC_LOW => self.selected_queue().map_or(0, |q| q.desc as u32 as u64),
            QUEUE_DESC_HIGH => self
                .selected_queue()
                .map_or(0, |q| (q.desc >> 32) as u32 as u64),
            QUEUE_DRIVER_LOW => self.selected_queue().map_or(0, |q| q.driver as u32 as u64),
            QUEUE_DRIVER_HIGH => self
                .selected_queue()
                .map_or(0, |q| (q.driver >> 32) as u32 as u64),
            QUEUE_DEVICE_LOW => self.selected_queue().map_or(0, |q| q.device as u32 as u64),
            QUEUE_DEVICE_HIGH => self
                .selected_queue()
                .map_or(0, |q| (q.device >> 32) as u32 as u64),
            CONFIG_GENERATION => 0,
            CONFIG_MAC..=CONFIG_MAC_END => self.mac[(offset - CONFIG_MAC) as usize] as u64,
            _ => 0,
        }
    }

    fn write(&mut self, offset: u64, _size: u8, value: u64) {
        match offset {
            DEVICE_FEATURES_SEL => self.device_features_sel = value as u32,
            DRIVER_FEATURES => {
                let shift = self.driver_features_sel * 32;
                let mask = 0xffff_ffffu64 << shift;
                self.driver_features =
                    (self.driver_features & !mask) | ((value as u32 as u64) << shift);
            }
            DRIVER_FEATURES_SEL => self.driver_features_sel = value as u32,
            QUEUE_SEL => self.queue_sel = value as u32,
            QUEUE_NUM => {
                if let Some(q) = self.selected_queue_mut() {
                    q.size = (value as u32).min(QUEUE_SIZE);
                }
            }
            QUEUE_READY => {
                if let Some(q) = self.selected_queue_mut() {
                    q.ready = value != 0;
                }
            }
            QUEUE_NOTIFY => {
                // log::info!("notify");
                self.notify_queue(value as u32)
            }
            INTERRUPT_ACK => self.interrupt_status &= !(value as u32),
            STATUS => {
                if value == 0 {
                    self.reset();
                } else {
                    self.status = value as u32;
                }
            }
            QUEUE_DESC_LOW => {
                if let Some(q) = self.selected_queue_mut() {
                    q.desc = (q.desc & !0xffff_ffff) | (value as u32 as u64);
                }
            }
            QUEUE_DESC_HIGH => {
                if let Some(q) = self.selected_queue_mut() {
                    q.desc = (q.desc & 0xffff_ffff) | ((value as u32 as u64) << 32);
                }
            }
            QUEUE_DRIVER_LOW => {
                if let Some(q) = self.selected_queue_mut() {
                    q.driver = (q.driver & !0xffff_ffff) | (value as u32 as u64);
                }
            }
            QUEUE_DRIVER_HIGH => {
                if let Some(q) = self.selected_queue_mut() {
                    q.driver = (q.driver & 0xffff_ffff) | ((value as u32 as u64) << 32);
                }
            }
            QUEUE_DEVICE_LOW => {
                if let Some(q) = self.selected_queue_mut() {
                    q.device = (q.device & !0xffff_ffff) | (value as u32 as u64);
                }
            }
            QUEUE_DEVICE_HIGH => {
                if let Some(q) = self.selected_queue_mut() {
                    q.device = (q.device & 0xffff_ffff) | ((value as u32 as u64) << 32);
                }
            }
            _ => {}
        }
    }
}
