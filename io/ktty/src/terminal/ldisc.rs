// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Terminal input semantics, independent of backend readiness and execution.
use alloc::{sync::Arc, vec::Vec};
use core::sync::atomic::{AtomicBool, Ordering};

use kerrno::KError;
use ksignal::SignalInfo;
use linux_raw_sys::general::{
    ECHOCTL, ECHOK, ICRNL, IGNCR, ISIG, VEOF, VERASE, VKILL, VMIN, VTIME,
};
use ringbuf::{
    CachingProd,
    traits::{Consumer, Observer, Producer},
};

use crate::{
    port::{ReadBuf, TtyWrite},
    terminal::{Terminal, termios::Termios2},
};

pub(crate) struct LineDiscipline<W> {
    terminal: Arc<Terminal>,

    writer: W,

    buf_tx: CachingProd<ReadBuf>,

    line_buf: Vec<u8>,
    line_read: Option<usize>,
    clear_line_buf: Arc<AtomicBool>,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum InputStop {
    InputEmpty,
    OutputFull,
    BudgetExhausted,
}

pub(crate) struct InputProgress {
    pub(crate) bytes_consumed: usize,
    pub(crate) bytes_published: usize,
    pub(crate) stop: InputStop,
}

impl<W: TtyWrite> LineDiscipline<W> {
    pub(crate) fn new(
        terminal: Arc<Terminal>,
        writer: W,
        buf_tx: CachingProd<ReadBuf>,
        clear_line_buf: Arc<AtomicBool>,
    ) -> Self {
        Self {
            terminal,
            writer,
            buf_tx,
            clear_line_buf,
            line_buf: Vec::new(),
            line_read: None,
        }
    }

    pub(crate) fn into_output(self) -> CachingProd<ReadBuf> {
        self.buf_tx
    }

    /// Consumes a prefix of supplied bytes and publishes terminal input.
    ///
    /// Task context only: echo and signal delivery may acquire task locks.
    /// The caller retains bytes not consumed and serializes receive calls.
    pub(crate) fn receive(
        &mut self,
        bytes: &[u8],
        input_budget: usize,
        output_budget: usize,
    ) -> InputProgress {
        let mut progress = InputProgress {
            bytes_consumed: 0,
            bytes_published: 0,
            stop: InputStop::InputEmpty,
        };
        if self.clear_line_buf.swap(false, Ordering::Relaxed) {
            self.line_buf.clear();
            self.line_read = None;
        }
        if input_budget == 0 || output_budget == 0 {
            progress.stop = InputStop::BudgetExhausted;
            return progress;
        }
        let term = self.terminal.load_termios();
        loop {
            if progress.bytes_consumed == input_budget || progress.bytes_published == output_budget
            {
                progress.stop = InputStop::BudgetExhausted;
                break;
            }
            if let Some(offset) = &mut self.line_read {
                let pending = &self.line_buf[*offset..];
                let count = pending.len().min(output_budget - progress.bytes_published);
                let read = self.buf_tx.push_slice(&pending[..count]);
                if read == 0 {
                    progress.stop = InputStop::OutputFull;
                    break;
                }
                progress.bytes_published += read;
                *offset += read;
                if *offset == self.line_buf.len() {
                    self.line_read = None;
                    self.line_buf.clear();
                }
                continue;
            }
            if self.buf_tx.is_full() {
                progress.stop = InputStop::OutputFull;
                break;
            }
            if progress.bytes_consumed == bytes.len() {
                break;
            }
            let mut ch = bytes[progress.bytes_consumed];
            progress.bytes_consumed += 1;

            if ch == b'\r' {
                if term.has_iflag(IGNCR) {
                    continue;
                }
                if term.has_iflag(ICRNL) {
                    ch = b'\n';
                }
            }

            self.check_send_signal(&term, ch);

            if term.echo() {
                self.output_char(&term, ch);
            }
            if !term.canonical() {
                #[expect(
                    clippy::unwrap_used,
                    reason = "CG-021-no-panic-on-reachable-failure: this sole producer checked \
                              is_full above and has not pushed since; the consumer can only free \
                              capacity"
                )]
                self.buf_tx.try_push(ch).unwrap();
                progress.bytes_published += 1;
                continue;
            }

            // Canonical mode
            if term.has_lflag(ECHOK) && ch == term.special_char(VKILL) {
                self.line_buf.clear();
                continue;
            }
            if ch == term.special_char(VERASE) {
                self.line_buf.pop();
                continue;
            }

            if term.is_eol(ch) || ch == term.special_char(VEOF) {
                if ch != term.special_char(VEOF) {
                    self.line_buf.push(ch);
                }
                if !self.line_buf.is_empty() {
                    self.line_read = Some(0);
                }
                continue;
            }

            if ch.is_ascii_graphic() || matches!(ch, b' ' | b'\t') {
                self.line_buf.push(ch);
                continue;
            }
        }

        progress
    }

    fn check_send_signal(&self, term: &Termios2, ch: u8) {
        if !term.canonical() || !term.has_lflag(ISIG) {
            return;
        }
        if let Some(signo) = term.signo_for(ch)
            && let Some(pg) = self.terminal.job_control.foreground()
        {
            let sig = SignalInfo::new_kernel(signo);
            if let Err(err) = kprocess::process_signals::send_to_process_group(pg.pgid(), Some(sig))
            {
                warn!("Failed to send signal: {err:?}");
            }
        }
    }

    fn output_char(&self, term: &Termios2, ch: u8) {
        match ch {
            b'\n' => self.writer.write(b"\n"),
            b'\r' => self.writer.write(b"\r\n"),
            ch if ch == term.special_char(VERASE) => self.writer.write(b"\x08 \x08"),
            ch if ch == b' ' || ch.is_ascii_graphic() => self.writer.write(&[ch]),
            ch if ch.is_ascii_control() && term.has_lflag(ECHOCTL) => {
                self.writer.write(&[b'^', (ch + 0x40)]);
            }
            other => {
                warn!("Ignored echo char: {other:#x}");
            }
        }
    }
}

/// VTIME is not modeled yet; preserve the single-byte approximation for both
/// reads and readiness checks until inter-byte timing is implemented.
fn effective_vmin(terminal: &Terminal) -> usize {
    let term = terminal.termios.lock();
    if term.canonical() || term.special_char(VTIME) > 0 {
        1
    } else {
        term.special_char(VMIN) as usize
    }
}

/// Interprets cooked-buffer readiness using the terminal's current read policy.
pub(crate) fn read_ready(terminal: &Terminal, input: &ringbuf::CachingCons<ReadBuf>) -> bool {
    if input.is_empty() {
        return false;
    }
    let vmin = effective_vmin(terminal);
    vmin == 0 || input.occupied_len() >= vmin
}

/// Consumes cooked bytes; scheduling more input remains the port's job.
pub(crate) fn read_processed(
    terminal: &Terminal,
    input: &mut ringbuf::CachingCons<ReadBuf>,
    buf: &mut [u8],
) -> kerrno::KResult<usize> {
    let vmin = effective_vmin(terminal);

    if buf.len() < vmin {
        return Err(KError::WouldBlock);
    }

    let available = input.occupied_len();
    if available == 0 || (vmin > 0 && available < vmin) {
        return Err(KError::WouldBlock);
    }

    let read = input.pop_slice(buf);
    Ok(read)
}
