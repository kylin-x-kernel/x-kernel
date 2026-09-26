// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::{boxed::Box, sync::Arc, vec::Vec};
use core::{
    future::poll_fn,
    ops::Range,
    sync::atomic::{AtomicBool, Ordering},
    task::Poll,
};

use console_driver::serial::InputWork;
use kcpu_id_map::LogicalCpuId;
use kerrno::{KError, KResult};
use kpoll::{PollContext, PollRegisterError, PollRegistrations, PollSet};
use ksignal::SignalInfo;
use ktask::future::block_on;
use kworkng::{SubmitError, WorkHandle};
use linux_raw_sys::general::{
    ECHOCTL, ECHOK, ICRNL, IGNCR, ISIG, VEOF, VERASE, VKILL, VMIN, VTIME,
};
use ringbuf::{
    CachingCons, CachingProd,
    traits::{Consumer, Observer, Producer, Split},
};

use crate::terminal::{Terminal, termios::Termios2};

const BUF_SIZE: usize = 80;
const INPUT_WORK_BUDGET_BYTES: usize = 256;

type ReadBuf = Arc<ringbuf::StaticRb<u8, BUF_SIZE>>;

type ExternalRegisterFn =
    Box<dyn for<'a> Fn(&'a mut PollContext<'_>) -> Result<(), PollRegisterError> + Send + Sync>;

/// How should we process inputs?
pub enum ProcessMode {
    /// Process inputs only on call to `read`
    ///
    /// This is the fallback strategy and is rather limited. For instance, you
    /// can't interrupt a running program by Ctrl+C unless it's not blocked on a
    /// `read` call to the terminal, since the signal is emitted only when
    /// inputs are being processed.
    Manual,
    /// Spawns task for processing inputs, relying on external events to wake
    /// up.
    ///
    /// In this mode a dedicated task processes inputs. When there's
    /// nothing to read the argument is invoked to register rx waker.
    External(ExternalRegisterFn),
    /// Processes input in finite task-context work rounds on a ready CPU.
    ///
    /// Construct in sleepable context after normal-worker startup.
    /// The owner must connect the
    /// work to its input source, submit an initial check, and stop submissions
    /// before permanently shutting down the work at teardown.
    Work(LogicalCpuId),
    /// Do not process inputs.
    ///
    /// This is only used by the master side of pseudo tty. The argument is the
    /// [`PollSet`] for incoming data.
    None(Arc<PollSet>),
}

pub struct TtyConfig<R, W> {
    pub reader: R,
    pub writer: W,
    pub process_mode: ProcessMode,
}

pub trait TtyRead: Send + Sync + 'static {
    fn read(&mut self, buf: &mut [u8]) -> usize;
}
pub trait TtyWrite: Send + Sync + 'static {
    fn write(&self, buf: &[u8]);
}

struct InputReader<R, W> {
    terminal: Arc<Terminal>,

    reader: R,
    writer: W,

    buf_tx: CachingProd<ReadBuf>,
    read_buf: [u8; BUF_SIZE],
    read_range: Range<usize>,

    line_buf: Vec<u8>,
    line_read: Option<usize>,
    clear_line_buf: Arc<AtomicBool>,
}

#[derive(Debug, PartialEq, Eq)]
enum InputStop {
    InputEmpty,
    OutputFull,
    BudgetExhausted,
}

struct InputProgress {
    bytes_consumed: usize,
    bytes_published: usize,
    stop: InputStop,
}

impl<R: TtyRead, W: TtyWrite> InputReader<R, W> {
    /// Pulls bytes from the reader into the line buffer and pushes
    /// processed bytes toward the consumer.
    ///
    /// # Panics
    ///
    /// Only via the internal `try_push(..).unwrap()` if the output ring
    /// were full; the `is_full` guard above the push excludes that path.
    pub fn poll(&mut self) -> bool {
        self.process_batch(usize::MAX, usize::MAX).bytes_published > 0
    }

    fn process_budget(&mut self, budget: usize) -> InputProgress {
        let mut total = InputProgress {
            bytes_consumed: 0,
            bytes_published: 0,
            stop: InputStop::InputEmpty,
        };
        loop {
            let progress = self.process_batch(
                budget - total.bytes_consumed,
                budget - total.bytes_published,
            );
            total.bytes_consumed += progress.bytes_consumed;
            total.bytes_published += progress.bytes_published;
            total.stop = progress.stop;
            if total.stop != InputStop::InputEmpty
                || (progress.bytes_consumed == 0 && progress.bytes_published == 0)
            {
                return total;
            }
        }
    }

    fn process_batch(&mut self, input_budget: usize, output_budget: usize) -> InputProgress {
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
        if self.read_range.is_empty() {
            let read = self.reader.read(&mut self.read_buf);
            self.read_range = 0..read;
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
            if self.read_range.is_empty() {
                break;
            }
            let mut ch = self.read_buf[self.read_range.start];
            self.read_range.start += 1;
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

struct SimpleReader<R> {
    reader: R,
    read_buf: [u8; BUF_SIZE],
    buf_tx: CachingProd<ReadBuf>,
}
impl<R: TtyRead> SimpleReader<R> {
    pub fn poll(&mut self) {
        let read = self.reader.read(&mut self.read_buf);
        for ch in &self.read_buf[..read] {
            if *ch == b'\n' {
                let _ = self.buf_tx.try_push(b'\r');
            }
            let _ = self.buf_tx.try_push(*ch);
        }
    }
}

enum Processor<R, W> {
    Manual(InputReader<R, W>),
    External(Arc<PollSet>),
    Work {
        work: InputWork,
        cpu_id: LogicalCpuId,
        poll_rx: Arc<PollSet>,
    },
    None(SimpleReader<R>, Arc<PollSet>),
}

fn queue_input_work(cpu_id: LogicalCpuId, work: &InputWork) {
    match work.queue_default(Some(cpu_id)) {
        Ok(_) | Err(SubmitError::AlreadyQueued | SubmitError::Closed) => {}
        error => warn!("TTY input work could not be queued: {error:?}"),
    }
}

pub struct LineDiscipline<R, W> {
    terminal: Arc<Terminal>,
    buf_rx: CachingCons<ReadBuf>,
    poll_tx: Arc<PollSet>,
    clear_line_buf: Arc<AtomicBool>,
    processor: Processor<R, W>,
}

impl<R: TtyRead, W: TtyWrite> LineDiscipline<R, W> {
    /// Creates input processing state without starting Work-mode callbacks.
    ///
    /// # Panics
    /// Work mode requires sleepable context and initialized normal workers.
    /// Allocation failure is fatal.
    pub fn new(terminal: Arc<Terminal>, config: TtyConfig<R, W>) -> Self {
        let (buf_tx, buf_rx) = ReadBuf::default().split();

        let clear_line_buf = Arc::new(AtomicBool::new(false));
        let mut reader = InputReader {
            terminal: terminal.clone(),

            reader: config.reader,
            writer: config.writer,

            buf_tx,
            read_buf: [0; BUF_SIZE],
            read_range: 0..0,

            line_buf: Vec::new(),
            line_read: None,
            clear_line_buf: clear_line_buf.clone(),
        };

        let poll_tx = Arc::new(PollSet::new());
        let processor = match config.process_mode {
            ProcessMode::Manual => Processor::Manual(reader),
            ProcessMode::External(register) => {
                let poll_rx = Arc::new(PollSet::new());
                ktask::spawn_with_name(
                    {
                        let poll_rx = poll_rx.clone();
                        let poll_tx = poll_tx.clone();
                        move || {
                            let mut registrations = PollRegistrations::new();
                            block_on(poll_fn(|cx| {
                                while reader.poll() {
                                    poll_rx.wake();
                                }
                                let mut context = registrations.context(cx);
                                if let Err(error) = context
                                    .register(&poll_tx)
                                    .and_then(|()| register(&mut context))
                                {
                                    warn!("Failed to register TTY input waiter: {error}");
                                    // Retry after scheduler yields; do not exit the reader.
                                    context.wake_by_ref();
                                    return Poll::Pending;
                                }
                                drop(context);
                                while reader.poll() {
                                    poll_rx.wake();
                                }
                                Poll::Pending
                            }))
                        }
                    },
                    "tty-reader".into(),
                );
                Processor::External(poll_rx)
            }
            ProcessMode::Work(cpu_id) => {
                let poll_rx = Arc::new(PollSet::new());
                let ready = poll_rx.clone();
                let reader = ksync::Mutex::new(reader);
                let work: InputWork = Arc::new_cyclic(|weak| {
                    // Requeue must not keep the callback's owner alive forever.
                    let weak = weak.clone();
                    let callback: Box<dyn Fn() + Send + Sync> = Box::new(move || {
                        let progress = reader.lock().process_budget(INPUT_WORK_BUDGET_BYTES);
                        if progress.bytes_published > 0 {
                            ready.wake();
                        }
                        if progress.stop == InputStop::BudgetExhausted
                            && let Some(work) = weak.upgrade()
                        {
                            queue_input_work(cpu_id, &work);
                        }
                    });
                    WorkHandle::new(callback).expect("TTY input requires normal workers")
                });
                Processor::Work {
                    work,
                    cpu_id,
                    poll_rx,
                }
            }
            ProcessMode::None(poll_rx) => {
                // Destruct the reader here
                Processor::None(
                    SimpleReader {
                        reader: reader.reader,
                        read_buf: [0; BUF_SIZE],
                        buf_tx: reader.buf_tx,
                    },
                    poll_rx,
                )
            }
        };
        Self {
            terminal,
            buf_rx,
            poll_tx,
            clear_line_buf,
            processor,
        }
    }

    pub fn drain_input(&mut self) {
        self.buf_rx.clear();
        self.clear_line_buf.store(true, Ordering::Relaxed);
        if let Processor::Work { cpu_id, work, .. } = &self.processor {
            queue_input_work(*cpu_id, work);
        }
    }

    /// Returns the work and its fixed queue binding for host input integration.
    /// Cloning it does not start processing or keep the line discipline alive.
    pub(crate) fn input_work(&self) -> Option<(InputWork, LogicalCpuId)> {
        match &self.processor {
            Processor::Work { work, cpu_id, .. } => Some((work.clone(), *cpu_id)),
            _ => None,
        }
    }

    pub fn poll_read(&mut self) -> bool {
        match &mut self.processor {
            Processor::Manual(reader) => {
                reader.poll();
            }
            Processor::None(reader, _) => reader.poll(),
            _ => {}
        }
        if self.buf_rx.is_empty() {
            return false;
        }
        let term = self.terminal.termios.lock().clone();
        let vmin = if term.canonical() {
            1
        } else {
            term.special_char(VMIN) as usize
        };
        vmin == 0 || self.buf_rx.occupied_len() >= vmin
    }

    pub fn register_rx(&self, context: &mut PollContext<'_>) -> Result<(), PollRegisterError> {
        match &self.processor {
            Processor::Manual(_) => {
                // Manual mode has no external wake source, so force an immediate
                // recheck by waking the current waiter.
                context.wake_by_ref();
            }
            Processor::External(set)
            | Processor::None(_, set)
            | Processor::Work { poll_rx: set, .. } => {
                context.register(set)?;
            }
        }
        Ok(())
    }

    pub fn read(&mut self, buf: &mut [u8]) -> KResult<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        if matches!(self.processor, Processor::None(_, _)) {
            let read = self.buf_rx.pop_slice(buf);
            return if read == 0 {
                Err(KError::WouldBlock)
            } else {
                Ok(read)
            };
        }

        let term = self.terminal.termios.lock().clone();
        let vmin = if term.canonical() {
            1
        } else {
            let vtime = term.special_char(VTIME);
            if vtime > 0 {
                todo!();
            }
            term.special_char(VMIN) as usize
        };

        if buf.len() < vmin {
            return Err(KError::WouldBlock);
        }

        let available = self.buf_rx.occupied_len();
        if available == 0 || (vmin > 0 && available < vmin) {
            return Err(KError::WouldBlock);
        }

        let read = self.buf_rx.pop_slice(buf);
        if let Processor::Work { cpu_id, work, .. } = &self.processor {
            queue_input_work(*cpu_id, work);
        } else {
            self.poll_tx.wake();
        }
        Ok(read)
    }
}

#[cfg(unittest)]
mod ldisc_tests {
    use alloc::{collections::VecDeque, sync::Arc, task::Wake, vec};
    use core::{
        mem,
        sync::atomic::{AtomicBool, Ordering},
        task::{Context, Waker},
    };

    use kpoll::{PollRegistrations, PollSet};
    use kspin::SpinNoIrq;
    use linux_raw_sys::general::{
        ECHO, ICANON, IGNCR, ISIG, IXON, ONLCR, OPOST, speed_t, tcflag_t,
    };
    use unittest::def_test;

    use super::*;
    use crate::terminal::termios;

    #[repr(C)]
    #[derive(Clone, Copy)]
    struct RawTermios {
        c_iflag: tcflag_t,
        c_oflag: tcflag_t,
        c_cflag: tcflag_t,
        c_lflag: tcflag_t,
        c_line: u8,
        c_cc: [u8; 19usize],
    }

    #[repr(C)]
    #[derive(Clone, Copy)]
    struct RawTermios2 {
        termios: RawTermios,
        c_ispeed: speed_t,
        c_ospeed: speed_t,
    }

    struct MockReader {
        data: Vec<u8>,
        cursor: usize,
        max_chunk_len: usize,
    }

    impl MockReader {
        fn new(data: &[u8]) -> Self {
            Self {
                data: data.to_vec(),
                cursor: 0,
                max_chunk_len: BUF_SIZE,
            }
        }

        fn with_chunk_limit(data: &[u8], max_chunk_len: usize) -> Self {
            Self {
                data: data.to_vec(),
                cursor: 0,
                max_chunk_len,
            }
        }
    }

    impl TtyRead for MockReader {
        fn read(&mut self, buf: &mut [u8]) -> usize {
            let remaining = self.data.len().saturating_sub(self.cursor);
            if remaining == 0 {
                return 0;
            }

            let read = remaining.min(buf.len()).min(self.max_chunk_len);
            buf[..read].copy_from_slice(&self.data[self.cursor..self.cursor + read]);
            self.cursor += read;
            read
        }
    }

    #[derive(Clone, Default)]
    struct MockWriter {
        writes: Arc<SpinNoIrq<VecDeque<Vec<u8>>>>,
    }

    impl MockWriter {
        fn written(&self) -> Vec<u8> {
            let guard = self.writes.lock();
            guard
                .iter()
                .flat_map(|chunk| chunk.iter().copied())
                .collect::<Vec<_>>()
        }
    }

    impl TtyWrite for MockWriter {
        fn write(&self, buf: &[u8]) {
            self.writes.lock().push_back(buf.to_vec());
        }
    }

    #[derive(Default)]
    struct WakeCounter {
        woke: AtomicBool,
    }

    impl Wake for WakeCounter {
        fn wake(self: Arc<Self>) {
            self.woke.store(true, Ordering::Relaxed);
        }

        fn wake_by_ref(self: &Arc<Self>) {
            self.woke.store(true, Ordering::Relaxed);
        }
    }

    fn set_termios(terminal: &Arc<Terminal>, update_fn: impl FnOnce(&mut RawTermios)) {
        // SAFETY: `RawTermios2` mirrors `Termios2` field-for-field with the
        // same `#[repr(C)]` layout, and we start from a valid `Termios2`
        // default before mutating only termios flag bytes and control chars.
        let mut raw = unsafe {
            mem::transmute::<termios::Termios2, RawTermios2>(termios::Termios2::default())
        };
        update_fn(&mut raw.termios);
        // SAFETY: `raw` was derived from a valid `Termios2` and only adjusted
        // through compatible `tcflag_t` and control-character fields.
        let termios = unsafe { mem::transmute::<RawTermios2, termios::Termios2>(raw) };
        *terminal.termios.lock() = Arc::new(termios);
    }

    fn new_manual_ldisc(
        data: &[u8],
        update_fn: impl FnOnce(&Arc<Terminal>),
    ) -> (LineDiscipline<MockReader, MockWriter>, MockWriter) {
        let terminal = Arc::new(Terminal::default());
        update_fn(&terminal);
        let writer = MockWriter::default();
        let ldisc = LineDiscipline::new(
            terminal,
            TtyConfig {
                reader: MockReader::new(data),
                writer: writer.clone(),
                process_mode: ProcessMode::Manual,
            },
        );
        (ldisc, writer)
    }

    fn new_manual_ldisc_with_chunk_limit(
        data: &[u8],
        max_chunk_len: usize,
        update_fn: impl FnOnce(&Arc<Terminal>),
    ) -> (LineDiscipline<MockReader, MockWriter>, MockWriter) {
        let terminal = Arc::new(Terminal::default());
        update_fn(&terminal);
        let writer = MockWriter::default();
        let ldisc = LineDiscipline::new(
            terminal,
            TtyConfig {
                reader: MockReader::with_chunk_limit(data, max_chunk_len),
                writer: writer.clone(),
                process_mode: ProcessMode::Manual,
            },
        );
        (ldisc, writer)
    }

    #[def_test]
    fn test_canonical_line_buffering_and_echo() {
        let (mut ldisc, writer) = new_manual_ldisc(b"ab\n", |_| {});

        assert!(ldisc.poll_read());

        let mut buf = [0; 8];
        assert_eq!(ldisc.read(&mut buf).unwrap(), 3);
        assert_eq!(&buf[..3], b"ab\n");
        assert_eq!(writer.written(), b"ab\n");
    }

    #[def_test]
    fn test_canonical_erase_updates_line_and_echo() {
        let erase = termios::Termios2::default().special_char(VERASE);
        let input = [b'a', b'b', erase, b'\n'];
        let (mut ldisc, writer) = new_manual_ldisc(&input, |_| {});

        assert!(ldisc.poll_read());

        let mut buf = [0; 8];
        assert_eq!(ldisc.read(&mut buf).unwrap(), 2);
        assert_eq!(&buf[..2], b"a\n");
        assert_eq!(writer.written(), b"ab\x08 \x08\n");
    }

    #[def_test]
    fn test_canonical_kill_clears_pending_line() {
        let kill = termios::Termios2::default().special_char(VKILL);
        let input = [b'a', b'b', kill, b'c', b'\n'];
        let (mut ldisc, writer) = new_manual_ldisc(&input, |_| {});

        assert!(ldisc.poll_read());

        let mut buf = [0; 8];
        assert_eq!(ldisc.read(&mut buf).unwrap(), 2);
        assert_eq!(&buf[..2], b"c\n");
        assert_eq!(writer.written(), vec![b'a', b'b', b'^', b'U', b'c', b'\n']);
    }

    #[def_test]
    fn test_canonical_eof_flushes_without_enqueuing_eof() {
        let eof = termios::Termios2::default().special_char(VEOF);
        let input = [b'a', b'b', eof];
        let (mut ldisc, writer) = new_manual_ldisc(&input, |_| {});

        assert!(ldisc.poll_read());

        let mut buf = [0; 8];
        assert_eq!(ldisc.read(&mut buf).unwrap(), 2);
        assert_eq!(&buf[..2], b"ab");
        assert_eq!(writer.written(), vec![b'a', b'b', b'^', b'D']);
    }

    #[def_test]
    fn test_empty_eof_does_not_make_data_available() {
        let eof = termios::Termios2::default().special_char(VEOF);
        let (mut ldisc, writer) = new_manual_ldisc(&[eof], |_| {});

        assert!(!ldisc.poll_read());

        let mut buf = [0; 4];
        assert!(matches!(ldisc.read(&mut buf), Err(KError::WouldBlock)));
        assert_eq!(writer.written(), vec![b'^', b'D']);
    }

    #[def_test]
    fn test_igncr_drops_carriage_return() {
        let (mut ldisc, writer) = new_manual_ldisc(b"\r", |terminal| {
            set_termios(terminal, |term| {
                term.c_iflag = IGNCR | IXON;
                term.c_oflag = OPOST | ONLCR;
            });
        });

        assert!(!ldisc.poll_read());

        let mut buf = [0; 4];
        assert!(matches!(ldisc.read(&mut buf), Err(KError::WouldBlock)));
        assert!(writer.written().is_empty());
    }

    #[def_test]
    fn test_noncanonical_vmin_blocks_until_enough_bytes() {
        let (mut ldisc, writer) = new_manual_ldisc_with_chunk_limit(b"ab", 1, |terminal| {
            set_termios(terminal, |term| {
                term.c_lflag &= !(ICANON | ISIG | ECHO);
                term.c_cc[VMIN as usize] = 2;
            });
        });

        assert!(!ldisc.poll_read());
        let mut buf = [0; 8];
        assert!(matches!(ldisc.read(&mut buf), Err(KError::WouldBlock)));

        assert!(ldisc.poll_read());
        assert_eq!(ldisc.read(&mut buf).unwrap(), 2);
        assert_eq!(&buf[..2], b"ab");
        assert!(writer.written().is_empty());
    }

    #[def_test]
    fn test_noncanonical_small_user_buffer_returns_would_block() {
        let (mut ldisc, _) = new_manual_ldisc(b"ab", |terminal| {
            set_termios(terminal, |term| {
                term.c_lflag &= !ICANON;
                term.c_cc[VMIN as usize] = 2;
            });
        });

        assert!(ldisc.poll_read());

        let mut buf = [0; 1];
        assert!(matches!(ldisc.read(&mut buf), Err(KError::WouldBlock)));
    }

    #[def_test]
    fn test_drain_input_clears_pending_canonical_line() {
        let (mut ldisc, _) = new_manual_ldisc_with_chunk_limit(b"abc\n", 2, |_| {});

        assert!(!ldisc.poll_read());

        ldisc.drain_input();

        assert!(ldisc.poll_read());

        let mut buf = [0; 8];
        assert_eq!(ldisc.read(&mut buf).unwrap(), 2);
        assert_eq!(&buf[..2], b"c\n");
    }

    #[def_test]
    fn test_process_mode_none_reads_raw_stream_and_reports_would_block_when_empty() {
        let poll_rx = Arc::new(PollSet::new());
        let terminal = Arc::new(Terminal::default());
        let writer = MockWriter::default();
        let mut ldisc = LineDiscipline::new(
            terminal,
            TtyConfig {
                reader: MockReader::new(b"a\n"),
                writer,
                process_mode: ProcessMode::None(poll_rx),
            },
        );

        assert!(ldisc.poll_read());

        let mut buf = [0; 8];
        assert_eq!(ldisc.read(&mut buf).unwrap(), 3);
        assert_eq!(&buf[..3], b"a\r\n");
        assert!(matches!(ldisc.read(&mut buf), Err(KError::WouldBlock)));
    }

    #[def_test]
    fn test_manual_register_rx_wakes_immediately() {
        let (ldisc, _) = new_manual_ldisc(b"", |_| {});
        let wake_counter = Arc::new(WakeCounter::default());
        let waker = Waker::from(wake_counter.clone());
        let cx = Context::from_waker(&waker);
        let mut registrations = PollRegistrations::new();
        let mut context = registrations.context(&cx);

        ldisc.register_rx(&mut context).unwrap();

        assert!(wake_counter.woke.load(Ordering::Relaxed));
    }

    fn await_input<R: TtyRead>(ldisc: &mut LineDiscipline<R, MockWriter>) {
        let mut registrations = PollRegistrations::new();
        block_on(poll_fn(|cx| {
            ldisc.register_rx(&mut registrations.context(cx)).unwrap();
            if ldisc.poll_read() {
                Poll::Ready(())
            } else {
                Poll::Pending
            }
        }));
    }

    #[def_test]
    fn work_drains_multiple_budgets_and_resumes_after_read_space() {
        for raw in [false, true] {
            let mut data = b"ab \t".repeat(INPUT_WORK_BUDGET_BYTES);
            data.push(b'\n');
            let terminal = Arc::new(Terminal::default());
            if raw {
                set_termios(&terminal, |term| term.c_lflag &= !(ICANON | ISIG | ECHO));
            }
            let mut ldisc = LineDiscipline::new(
                terminal,
                TtyConfig {
                    reader: MockReader::with_chunk_limit(&data, 7),
                    writer: MockWriter::default(),
                    process_mode: ProcessMode::Work(khal::percpu::this_cpu_id()),
                },
            );
            let (work, cpu_id) = ldisc.input_work().unwrap();
            let weak = Arc::downgrade(&work);
            ldisc.drain_input();
            let mut received = Vec::new();
            while received.len() < data.len() {
                await_input(&mut ldisc);
                let mut bytes = [0; 11];
                let count = ldisc.read(&mut bytes).unwrap();
                received.extend_from_slice(&bytes[..count]);
            }
            work.shutdown().unwrap();
            assert_eq!(received, data);
            ldisc.drain_input();
            assert_eq!(work.queue_default(Some(cpu_id)), Err(SubmitError::Closed));
            assert!(!ldisc.poll_read());
            drop(ldisc);
            drop(work);
            assert!(weak.upgrade().is_none());
        }
    }

    #[def_test]
    fn input_and_pending_output_each_obey_the_budget() {
        let (mut ldisc, _) = new_manual_ldisc(&[b'a'; 600], |_| {});
        let Processor::Manual(reader) = &mut ldisc.processor else {
            unreachable!()
        };
        let progress = reader.process_budget(256);
        assert_eq!(progress.bytes_consumed, 256);
        assert_eq!(progress.bytes_published, 0);
        assert_eq!(progress.stop, InputStop::BudgetExhausted);
        reader.line_read = Some(0);
        let progress = reader.process_budget(7);
        assert_eq!(progress.bytes_published, 7);
        assert_eq!(progress.stop, InputStop::BudgetExhausted);
        reader.clear_line_buf.store(true, Ordering::Relaxed);
        reader.process_budget(0);
        assert!(reader.line_read.is_none());
    }

    #[def_test]
    fn running_empty_read_keeps_a_concurrent_submission() {
        struct Reader {
            entered: Arc<AtomicBool>,
            release: Arc<AtomicBool>,
            gate: Arc<ktask::WaitQueue>,
            source: MockReader,
        }
        impl TtyRead for Reader {
            fn read(&mut self, buf: &mut [u8]) -> usize {
                assert!(ktask::is_sleepable_context());
                if !self.entered.swap(true, Ordering::AcqRel) {
                    self.gate.notify_all(false);
                    self.gate
                        .wait_until(|| self.release.load(Ordering::Acquire));
                    return 0;
                }
                self.source.read(buf)
            }
        }
        let entered = Arc::new(AtomicBool::new(false));
        let release = Arc::new(AtomicBool::new(false));
        let gate = Arc::new(ktask::WaitQueue::new());
        let mut ldisc = LineDiscipline::new(
            Arc::default(),
            TtyConfig {
                reader: Reader {
                    entered: entered.clone(),
                    release: release.clone(),
                    gate: gate.clone(),
                    source: MockReader::new(b"z\n"),
                },
                writer: MockWriter::default(),
                process_mode: ProcessMode::Work(khal::percpu::this_cpu_id()),
            },
        );
        let (work, cpu) = ldisc.input_work().unwrap();
        queue_input_work(cpu, &work);
        gate.wait_until(|| entered.load(Ordering::Acquire));
        let submitted = work.queue_default(Some(cpu));
        release.store(true, Ordering::Release);
        gate.notify_all(false);
        await_input(&mut ldisc);
        let mut bytes = [0; 8];
        let count = ldisc.read(&mut bytes).unwrap();
        work.shutdown().unwrap();
        assert!(submitted.is_ok());
        assert_eq!(&bytes[..count], b"z\n");
    }
}
