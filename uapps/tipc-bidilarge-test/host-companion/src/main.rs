// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Host CA that sends 10 MiB to the TA and then receives 10 MiB back.

use std::{env, io, process, time::Instant};

use libtrusty_rs::TipcChannel;

const SERVICE: &str = "com.xkernel.test.tipc.bidi-large";
const TOTAL_BYTES: usize = 10 * 1024 * 1024;
const DEFAULT_UPLOAD_CHUNK_BYTES: usize = 4 * 1024;
const MAX_UPLOAD_CHUNK_BYTES: usize = 8 * 1024;
// The old 10 ms bridge backoff delivered about 1.55 MB/s; 5 MB/s catches that
// regression while leaving ample margin for slower QEMU TCG hosts.
const MIN_UPLOAD_MBPS: f64 = 5.0;
const START_UPLOAD: u8 = 1;
const UPLOAD_READY: u8 = 2;
const UPLOAD_DONE: u8 = 3;
const START_DOWNLOAD: u8 = 4;
const DOWNLOAD_DONE: u8 = 5;
const FINISHED: u8 = 6;

fn main() {
    if let Err(error) = run() {
        eprintln!("tipc-bidi-large-client: {error}");
        process::exit(1);
    }
}

fn run() -> io::Result<()> {
    let mut args = env::args().skip(1);
    let cid = args.next().unwrap_or_else(|| "103".into());
    let upload_chunk_bytes = args
        .next()
        .map(|value| {
            value.parse::<usize>().map_err(|error| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("invalid upload chunk size: {error}"),
                )
            })
        })
        .transpose()?
        .unwrap_or(DEFAULT_UPLOAD_CHUNK_BYTES);
    if !(1..=MAX_UPLOAD_CHUNK_BYTES).contains(&upload_chunk_bytes) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("upload chunk size must be between 1 and {MAX_UPLOAD_CHUNK_BYTES} bytes"),
        ));
    }
    let mut channel = TipcChannel::connect(&format!("VSOCK:{cid}:0"), SERVICE)
        .map_err(|error| io::Error::new(io::ErrorKind::ConnectionRefused, error))?;

    // Keep control messages separate so the data phase starts only after READY.
    channel.send(&[START_UPLOAD])?;
    expect_byte(&mut channel, UPLOAD_READY)?;
    let upload_started = Instant::now();
    let data = vec![0xA5; upload_chunk_bytes];
    for offset in (0..TOTAL_BYTES).step_by(upload_chunk_bytes) {
        channel.send(&data[..(TOTAL_BYTES - offset).min(upload_chunk_bytes)])?;
    }
    expect_byte(&mut channel, UPLOAD_DONE)?;
    let upload_mbps =
        (TOTAL_BYTES as f64 / 1_000_000.0) / upload_started.elapsed().as_secs_f64();
    println!("CA -> TA: sent {TOTAL_BYTES} bytes, {upload_mbps:.2} MB/s");
    if upload_mbps < MIN_UPLOAD_MBPS {
        return Err(io::Error::other(format!(
            "CA -> TA throughput {upload_mbps:.2} MB/s is below {MIN_UPLOAD_MBPS:.2} MB/s"
        )));
    }

    channel.send(&[START_DOWNLOAD])?;
    let mut buffer = [0; 64 * 1024];
    let mut received = 0;
    while received < TOTAL_BYTES {
        let length = (TOTAL_BYTES - received).min(buffer.len());
        let count = channel.recv_no_alloc(&mut buffer[..length])?;
        if count == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "TA closed the connection early",
            ));
        }
        received += count;
    }
    // The TA stops its endpoint TX timer only after receiving this notification.
    channel.send(&[DOWNLOAD_DONE])?;
    expect_byte(&mut channel, FINISHED)?;
    println!("TA -> CA: received {received} bytes");
    Ok(())
}

fn expect_byte(channel: &mut TipcChannel, expected: u8) -> io::Result<()> {
    let mut byte = [0];
    let count = channel.recv_no_alloc(&mut byte)?;
    if count != 1 || byte[0] != expected {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "invalid control message: received {byte:?} ({count} bytes), expected {expected}"
            ),
        ));
    }
    Ok(())
}
