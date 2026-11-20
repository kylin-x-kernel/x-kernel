// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! TA for measuring contiguous 10 MiB RX and TX throughput.

extern crate alloc;

use std::{cell::Cell, time::Instant};

use rust_libtipc::{
    ConnectResult, Handle, Manager, MessageResult, PortCfg, UnbufferedService, Uuid,
};

const SERVICE: &str = "com.xkernel.test.tipc.bidi-large";
const TOTAL_BYTES: usize = 10 * 1024 * 1024;
// Linux vhost-vsock accepts at most 64 KiB of payload in one packet.
const CHUNK_BYTES: usize = 4 * 1024;
// Match X-Kernel's TIPC receive-buffer limit (`IPC_CHAN_MAX_BUF_SIZE`). The
// vsock RX buffer is also 8 KiB, including its 44-byte header, so one forwarded
// RX event carries at most 8148 payload bytes and fits in one receive slot.
const RECEIVE_BYTES: usize = 8 * 1024;
const START_UPLOAD: u8 = 1;
const UPLOAD_READY: u8 = 2;
const UPLOAD_DONE: u8 = 3;
const START_DOWNLOAD: u8 = 4;
const DOWNLOAD_DONE: u8 = 5;
const FINISHED: u8 = 6;

#[derive(Clone, Copy)]
enum Phase {
    WaitingForUpload,
    Receiving {
        bytes: usize,
        messages: usize,
        started: Option<Instant>,
    },
    WaitingForDownload,
    WaitingForDownloadDone {
        started: Instant,
    },
    Complete,
}

struct ThroughputService;

impl UnbufferedService for ThroughputService {
    type Connection = Cell<Phase>;

    fn on_connect(
        &self,
        _port: &PortCfg,
        _channel: &Handle,
        _peer: &Uuid,
    ) -> rust_libtipc::Result<ConnectResult<Self::Connection>> {
        Ok(ConnectResult::Accept(Cell::new(Phase::WaitingForUpload)))
    }

    fn on_message(
        &self,
        phase: &Self::Connection,
        channel: &Handle,
        buffer: &mut [u8],
    ) -> rust_libtipc::Result<MessageResult> {
        let count = channel.recv_into(buffer)?;
        match phase.get() {
            Phase::WaitingForUpload if buffer[..count] == [START_UPLOAD] => {
                channel.send_vectored(&[&[UPLOAD_READY]], &[])?;
                phase.set(Phase::Receiving {
                    bytes: 0,
                    messages: 0,
                    started: None,
                });
            }
            Phase::Receiving {
                bytes,
                messages,
                started,
            } => {
                let started = started.unwrap_or_else(Instant::now);
                let bytes = bytes + count;
                let messages = messages + 1;
                if bytes > TOTAL_BYTES {
                    println!("tipc-bidi-large-server: RX exceeded {TOTAL_BYTES} bytes");
                    return Ok(MessageResult::CloseConnection);
                }
                if bytes == TOTAL_BYTES {
                    println!(
                        "tipc-bidi-large-server: RX {TOTAL_BYTES} bytes, {messages} messages, \
                         {:.2} MB/s",
                        mb_per_second(started)
                    );
                    channel.send_vectored(&[&[UPLOAD_DONE]], &[])?;
                    phase.set(Phase::WaitingForDownload);
                } else {
                    phase.set(Phase::Receiving {
                        bytes,
                        messages,
                        started: Some(started),
                    });
                }
            }
            Phase::WaitingForDownload if buffer[..count] == [START_DOWNLOAD] => {
                let started = Instant::now();
                let data = [0x5A; CHUNK_BYTES];
                for offset in (0..TOTAL_BYTES).step_by(CHUNK_BYTES) {
                    let length = (TOTAL_BYTES - offset).min(CHUNK_BYTES);
                    channel.send_vectored(&[&data[..length]], &[])?;
                }
                phase.set(Phase::WaitingForDownloadDone { started });
            }
            Phase::WaitingForDownloadDone { started } if buffer[..count] == [DOWNLOAD_DONE] => {
                println!(
                    "tipc-bidi-large-server: TX {TOTAL_BYTES} bytes, {:.2} MB/s",
                    mb_per_second(started)
                );
                channel.send_vectored(&[&[FINISHED]], &[])?;
                phase.set(Phase::Complete);
            }
            _ => {
                println!("tipc-bidi-large-server: unexpected message, length {count}");
                return Ok(MessageResult::CloseConnection);
            }
        }
        Ok(MessageResult::MaintainConnection)
    }

    fn max_message_length(&self) -> usize {
        RECEIVE_BYTES
    }
}

/// Returns decimal MB/s so the result matches `vsock_perf`.
fn mb_per_second(started: Instant) -> f64 {
    (TOTAL_BYTES as f64 / 1_000_000.0) / started.elapsed().as_secs_f64()
}

fn main() {
    if let Err(error) = run() {
        eprintln!("tipc-bidi-large-server: {error:?}");
        std::process::exit(1);
    }
}

fn run() -> rust_libtipc::Result<()> {
    let port = PortCfg::new(SERVICE)?
        .allow_ns_connect()
        .msg_queue_len(3)
        .msg_max_size(RECEIVE_BYTES as u32);
    rust_libtipc::service_dispatcher! {
        pub(crate) enum ThroughputDispatcher {
            ThroughputService,
        }
    }
    let mut dispatcher = ThroughputDispatcher::<1>::new()?;
    dispatcher.add_service(std::rc::Rc::new(ThroughputService), port)?;
    let manager =
        Manager::<_, Vec<u8>, 1, 8>::new_with_dispatcher(dispatcher, vec![0; RECEIVE_BYTES])?;
    println!("tipc-bidi-large-server: ready ({SERVICE})");
    manager.run_event_loop()
}
