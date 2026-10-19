// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::{sync::Arc, vec::Vec};
use core::sync::atomic::{fence, Ordering};
use kspin::SpinNoIrq;
use unittest::{assert_eq, def_test};
use virtio_drivers::{
    PhysAddr,
    transport::{DeviceStatus, DeviceType, InterruptStatus, Transport},
};
use zerocopy::{FromBytes, Immutable, IntoBytes};

use super::{CtrlHeader, Rect};
use crate::mock_virtio::{MockHal, MockQueue, MockTransport};

#[repr(C)]
#[derive(Clone, Copy)]
struct Descriptor {
    address: u64,
    len: u32,
    flags: u16,
    next: u16,
}

#[derive(Debug, Eq, PartialEq)]
enum GpuCommand {
    Transfer { rect: Rect, offset: u64, resource_id: u32 },
    SetScanout { rect: Rect, scanout_id: u32, resource_id: u32 },
    Flush { rect: Rect, resource_id: u32 },
    DetachBacking { resource_id: u32 },
    Unref { resource_id: u32 },
}

#[derive(Default)]
struct GpuQueue {
    ring: MockQueue,
    submitted: u16,
    completed: u16,
    commands: Vec<GpuCommand>,
    fail_next_flush: bool,
}

impl GpuQueue {
    fn complete_pending(&mut self) {
        let queue_size = self.ring.size;
        core::assert!(queue_size != 0);
        let available = self.ring.available as *const u16;
        let available_index = available.wrapping_add(1);
        // SAFETY: queue_set retains the live aligned avail ring until unset.
        let end = unsafe { available_index.read_volatile() };
        while self.submitted != end {
            let slot = available.wrapping_add(2 + usize::from(self.submitted) % queue_size);
            // SAFETY: modulo queue depth selects a live avail-ring slot.
            let token = unsafe { slot.read_volatile() };
            self.complete(token);
            self.submitted = self.submitted.wrapping_add(1);
        }
    }

    fn complete(&mut self, token: u16) {
        let queue = &self.ring;
        core::assert!(usize::from(token) < queue.size);
        let descriptors = queue.descriptors as *const Descriptor;
        let descriptor = descriptors.wrapping_add(usize::from(token));
        // SAFETY: token is bounded by the live descriptor table depth.
        let head = unsafe { descriptor.read_volatile() };
        core::assert_eq!(head.flags & 4, 0, "GPU test queue is configured without indirect descriptors");
        core::assert!(head.len >= 24, "GPU command header must be complete");
        let request = head.address as *const u8;
        core::assert!(head.flags & 1 != 0, "GPU request descriptor must chain to the response");
        let response_descriptor = descriptors.wrapping_add(usize::from(head.next));
        // SAFETY: the queue retains the request descriptor buffer through completion.
        let response_desc = unsafe { response_descriptor.read_volatile() };
        core::assert!(response_desc.flags & 2 != 0, "GPU response descriptor must be device-writable");
        let response = response_desc.address as *mut u8;

        // SAFETY: the live descriptor retains this initialized request buffer
        // through synchronous completion, and the slice is used only here.
        let request_bytes = unsafe { core::slice::from_raw_parts(request, head.len as usize) };
        let command_id = CtrlHeader::read_from_prefix(request_bytes)
            .unwrap()
            .0
            .hdr_type
            .0;
        let reply = match command_id {
            0x105 => {
                core::assert!(head.len >= 56, "TRANSFER_TO_HOST_2D request must be complete");
                let request = super::TransferToHost2D::read_from_prefix(request_bytes)
                    .unwrap()
                    .0;
                self.commands.push(GpuCommand::Transfer {
                    rect: request.rect,
                    offset: request.offset,
                    resource_id: request.resource_id,
                });
                super::Command::OK_NODATA
            }
            0x103 => {
                core::assert!(head.len >= 48, "SET_SCANOUT request must be complete");
                let request = super::SetScanout::read_from_prefix(request_bytes)
                    .unwrap()
                    .0;
                self.commands.push(GpuCommand::SetScanout {
                    rect: request.rect,
                    scanout_id: request.scanout_id,
                    resource_id: request.resource_id,
                });
                super::Command::OK_NODATA
            }
            0x104 => {
                core::assert!(head.len >= 48, "RESOURCE_FLUSH request must be complete");
                let request = super::ResourceFlush::read_from_prefix(request_bytes)
                    .unwrap()
                    .0;
                self.commands.push(GpuCommand::Flush {
                    rect: request.rect,
                    resource_id: request.resource_id,
                });
                if core::mem::take(&mut self.fail_next_flush) {
                    super::Command(0x1200)
                } else {
                    super::Command::OK_NODATA
                }
            }
            0x107 => {
                let request = super::ResourceDetachBacking::read_from_prefix(request_bytes)
                    .unwrap()
                    .0;
                self.commands.push(GpuCommand::DetachBacking {
                    resource_id: request.resource_id,
                });
                super::Command::OK_NODATA
            }
            0x102 => {
                let request = super::ResourceUnref::read_from_prefix(request_bytes)
                    .unwrap()
                    .0;
                self.commands.push(GpuCommand::Unref {
                    resource_id: request.resource_id,
                });
                super::Command::OK_NODATA
            }
            0x100 => super::Command::OK_DISPLAY_INFO,
            _ => core::panic!("unexpected GPU command {command_id:#x}"),
        };

        // SAFETY: the response descriptor grants this device the advertised writable capacity.
        unsafe { response.write_bytes(0, response_desc.len as usize) };
        // SAFETY: CtrlHeader's byte representation is the response wire format.
        let header = CtrlHeader::with_type(reply);
        // SAFETY: the response buffer is writable and at least a header long.
        unsafe { core::ptr::copy_nonoverlapping(header.as_bytes().as_ptr(), response, header.as_bytes().len()) };
        if command_id == 0x100 {
            // GET_DISPLAY_INFO's response appends the 16-byte rect and two u32 fields.
            // SAFETY: this response descriptor is PAGE_SIZE bytes, as allocated by the driver.
            unsafe { response.wrapping_add(32).cast::<u32>().write_unaligned(1024u32.to_le()) };
            // SAFETY: the same bounded display-info response includes its height field.
            unsafe { response.wrapping_add(36).cast::<u32>().write_unaligned(768u32.to_le()) };
            // SAFETY: the same bounded display-info response includes its enabled field.
            unsafe { response.wrapping_add(40).cast::<u32>().write_unaligned(1u32.to_le()) };
        }

        let used = queue.used as *mut u8;
        let slot = used.wrapping_add(4 + usize::from(self.completed) % queue.size * 8);
        // SAFETY: the live used ring is aligned and modulo depth selects an
        // in-bounds eight-byte used element after its four-byte header.
        unsafe { slot.cast::<u32>().write_volatile(u32::from(token)) };
        let length = slot.wrapping_add(4).cast::<u32>();
        // SAFETY: the length field follows the token within the same bounded used element.
        unsafe { length.write_volatile(response_desc.len) };
        fence(Ordering::Release);
        self.completed = self.completed.wrapping_add(1);
        let used_index = used.wrapping_add(2).cast::<u16>();
        // SAFETY: byte offset two is the aligned u16 used-ring index.
        unsafe { used_index.write_volatile(self.completed) };
    }
}

struct GpuTestTransport {
    base: MockTransport,
    queue: Arc<SpinNoIrq<GpuQueue>>,
}

impl Transport for GpuTestTransport {
    fn device_type(&self) -> DeviceType { self.base.device_type() }
    fn read_device_features(&mut self) -> u64 { self.base.read_device_features() }
    fn write_driver_features(&mut self, features: u64) { self.base.write_driver_features(features) }
    fn max_queue_size(&mut self, queue: u16) -> u32 { self.base.max_queue_size(queue) }
    fn notify(&mut self, queue: u16) {
        if queue == super::QUEUE_TRANSMIT { self.queue.lock().complete_pending(); }
    }
    fn get_status(&self) -> DeviceStatus { self.base.get_status() }
    fn set_status(&mut self, status: DeviceStatus) { self.base.set_status(status) }
    fn set_guest_page_size(&mut self, size: u32) { self.base.set_guest_page_size(size) }
    fn requires_legacy_layout(&self) -> bool { false }
    fn queue_set(&mut self, queue: u16, size: u32, descriptors: PhysAddr, available: PhysAddr, used: PhysAddr) {
        if queue == super::QUEUE_TRANSMIT {
            self.queue.lock().ring = MockQueue { size: size as usize, descriptors: descriptors as usize, available: available as usize, used: used as usize };
        }
    }
    fn queue_unset(&mut self, queue: u16) {
        if queue == super::QUEUE_TRANSMIT { self.queue.lock().ring = MockQueue::default(); }
    }
    fn queue_used(&mut self, _queue: u16) -> bool { false }
    fn ack_interrupt(&mut self) -> InterruptStatus { self.base.ack_interrupt() }
    fn read_config_generation(&self) -> u32 { self.base.read_config_generation() }
    fn read_config_space<T: FromBytes + IntoBytes>(&self, offset: usize) -> virtio_drivers::Result<T> {
        self.base.read_config_space(offset)
    }
    fn write_config_space<T: IntoBytes + Immutable>(&mut self, offset: usize, value: T) -> virtio_drivers::Result<()> {
        self.base.write_config_space(offset, value)
    }
}

fn new_gpu() -> (super::VirtIoGpu2d<MockHal, GpuTestTransport>, Arc<SpinNoIrq<GpuQueue>>) {
    let base = MockTransport::new_with_type(DeviceType::GPU);
    base.config_space.borrow_mut()[8..12].copy_from_slice(&1u32.to_le_bytes());
    let queue = Arc::new(SpinNoIrq::new(GpuQueue::default()));
    let transport = GpuTestTransport { base, queue: queue.clone() };
    let mut gpu = super::VirtIoGpu2d::new(transport).unwrap();
    core::assert_eq!(gpu.resolution().unwrap(), (1024, 768));
    (gpu, queue)
}

#[def_test]
fn production_present_reuses_binding_and_emits_scanout_zero_offset_commands() {
    let (mut gpu, queue) = new_gpu();
    let rect = super::ScanoutRect { x: 4, y: 8, width: 640, height: 480 };
    gpu.present_resource(7, rect).unwrap();
    gpu.present_resource(7, rect).unwrap();

    let commands = &queue.lock().commands;
    assert_eq!(commands.len(), 5);
    assert_eq!(commands[0], GpuCommand::Transfer { rect: Rect { x: 4, y: 8, width: 640, height: 480 }, offset: 0, resource_id: 7 });
    assert_eq!(commands[1], GpuCommand::SetScanout { rect: Rect { x: 4, y: 8, width: 640, height: 480 }, scanout_id: 0, resource_id: 7 });
    assert_eq!(commands[2], GpuCommand::Flush { rect: Rect { x: 4, y: 8, width: 640, height: 480 }, resource_id: 7 });
    assert_eq!(commands[3], commands[0]);
    assert_eq!(commands[4], commands[2]);
}

#[def_test]
fn production_present_clears_binding_after_flush_error_and_rebinds_on_retry() {
    let (mut gpu, queue) = new_gpu();
    let rect = super::ScanoutRect { x: 0, y: 0, width: 1024, height: 768 };
    queue.lock().fail_next_flush = true;

    assert_eq!(gpu.present_resource(3, rect), Err(virtio_drivers::Error::IoError));
    gpu.present_resource(3, rect).unwrap();

    let commands = &queue.lock().commands;
    assert_eq!(commands.len(), 6);
    assert_eq!(commands[1], GpuCommand::SetScanout { rect: Rect { x: 0, y: 0, width: 1024, height: 768 }, scanout_id: 0, resource_id: 3 });
    assert_eq!(commands[4], commands[1]);
}

#[def_test]
fn production_scanout_binding_tracks_resource_and_rect_through_destroy() {
    let (mut gpu, queue) = new_gpu();
    let rect = super::ScanoutRect {
        x: 4,
        y: 8,
        width: 640,
        height: 480,
    };
    let resized_rect = super::ScanoutRect {
        width: 800,
        ..rect
    };

    gpu.present_resource(7, rect).unwrap();
    gpu.present_resource(7, resized_rect).unwrap();
    gpu.present_resource(8, resized_rect).unwrap();
    gpu.destroy_resource(7).unwrap();
    gpu.present_resource(8, resized_rect).unwrap();
    gpu.destroy_resource(8).unwrap();
    gpu.present_resource(8, resized_rect).unwrap();

    let commands = &queue.lock().commands;
    assert_eq!(commands.len(), 18);
    assert_eq!(
        commands[1],
        GpuCommand::SetScanout {
            rect: Rect {
                x: 4,
                y: 8,
                width: 640,
                height: 480,
            },
            scanout_id: 0,
            resource_id: 7,
        }
    );
    assert_eq!(
        commands[4],
        GpuCommand::SetScanout {
            rect: Rect {
                x: 4,
                y: 8,
                width: 800,
                height: 480,
            },
            scanout_id: 0,
            resource_id: 7,
        }
    );
    assert_eq!(
        commands[7],
        GpuCommand::SetScanout {
            rect: Rect {
                x: 4,
                y: 8,
                width: 800,
                height: 480,
            },
            scanout_id: 0,
            resource_id: 8,
        }
    );
    assert_eq!(commands[9], GpuCommand::DetachBacking { resource_id: 7 });
    assert_eq!(commands[10], GpuCommand::Unref { resource_id: 7 });
    assert_eq!(commands[11], commands[6]);
    assert_eq!(commands[12], commands[8]);
    assert_eq!(commands[13], GpuCommand::DetachBacking { resource_id: 8 });
    assert_eq!(commands[14], GpuCommand::Unref { resource_id: 8 });
    assert_eq!(
        commands[16],
        GpuCommand::SetScanout {
            rect: Rect {
                x: 4,
                y: 8,
                width: 800,
                height: 480,
            },
            scanout_id: 0,
            resource_id: 8,
        }
    );
}
