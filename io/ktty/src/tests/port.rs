// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::{collections::VecDeque, sync::Arc, task::Wake, vec, vec::Vec};
use core::{
    mem,
    sync::atomic::{AtomicBool, Ordering},
    task::{Context, Waker},
};

use kpoll::{PollRegistrations, PollSet};
use kspin::SpinNoIrq;
use linux_raw_sys::general::{
    ECHO, ICANON, IGNCR, ISIG, IXON, ONLCR, OPOST, VEOF, VERASE, VKILL, VMIN, speed_t, tcflag_t,
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
    let mut raw =
        unsafe { mem::transmute::<termios::Termios2, RawTermios2>(termios::Termios2::default()) };
    update_fn(&mut raw.termios);
    // SAFETY: `raw` was derived from a valid `Termios2` and only adjusted
    // through compatible `tcflag_t` and control-character fields.
    let termios = unsafe { mem::transmute::<RawTermios2, termios::Termios2>(raw) };
    *terminal.termios.lock() = Arc::new(termios);
}

fn new_manual_ldisc(
    data: &[u8],
    update_fn: impl FnOnce(&Arc<Terminal>),
) -> (TtyPort<MockReader, MockWriter>, MockWriter) {
    let terminal = Arc::new(Terminal::default());
    update_fn(&terminal);
    let writer = MockWriter::default();
    let ldisc = TtyPort::new(
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
) -> (TtyPort<MockReader, MockWriter>, MockWriter) {
    let terminal = Arc::new(Terminal::default());
    update_fn(&terminal);
    let writer = MockWriter::default();
    let ldisc = TtyPort::new(
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
fn test_noncanonical_vtime_reads_single_byte() {
    let (mut input, _) = new_manual_ldisc(b"a", |terminal| {
        set_termios(terminal, |term| {
            term.c_lflag &= !ICANON;
            term.c_cc[VMIN as usize] = 1;
            term.c_cc[linux_raw_sys::general::VTIME as usize] = 1;
        });
    });
    assert!(input.poll_read());
    let mut buf = [0; 8];
    assert_eq!(input.read(&mut buf).unwrap(), 1);
    assert_eq!(&buf[..1], b"a");
    assert!(!input.poll_read());
    assert!(matches!(input.read(&mut buf), Err(KError::WouldBlock)));
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
    let mut ldisc = TtyPort::new(
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

fn await_input<R: TtyRead>(ldisc: &mut TtyPort<R, MockWriter>) {
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
fn discipline_accepts_bytes_without_a_backend_or_worker() {
    let terminal = Arc::new(Terminal::default());
    let writer = MockWriter::default();
    let (producer, mut consumer) = ReadBuf::default().split();
    let mut discipline =
        LineDiscipline::new(terminal, writer, producer, Arc::new(AtomicBool::new(false)));
    let progress = discipline.receive(b"abc\n", 4, 1);
    assert_eq!(progress.bytes_consumed, 4);
    assert_eq!(progress.bytes_published, 0);
    let progress = discipline.receive(&[], 8, 2);
    assert_eq!(progress.bytes_consumed, 0);
    assert_eq!(progress.bytes_published, 2);
    let mut result = [0; 4];
    assert_eq!(consumer.pop_slice(&mut result), 2);
    let progress = discipline.receive(&[], 8, 8);
    assert_eq!(progress.bytes_published, 2);
    assert_eq!(consumer.pop_slice(&mut result[2..]), 2);
    assert_eq!(&result, b"abc\n");
}

#[def_test]
fn discipline_reports_unconsumed_suffix_when_output_is_full() {
    let terminal = Arc::new(Terminal::default());
    set_termios(&terminal, |term| term.c_lflag &= !(ICANON | ISIG | ECHO));
    let (producer, mut consumer) = ReadBuf::default().split();
    let mut discipline = LineDiscipline::new(
        terminal,
        MockWriter::default(),
        producer,
        Arc::new(AtomicBool::new(false)),
    );
    let data = [b'x'; BUF_SIZE + 3];
    let progress = discipline.receive(&data, data.len(), data.len());
    assert_eq!(progress.bytes_consumed, BUF_SIZE);
    assert_eq!(progress.stop, InputStop::OutputFull);
    let mut output = [0; BUF_SIZE];
    assert_eq!(consumer.pop_slice(&mut output), BUF_SIZE);
    assert_eq!(output, [b'x'; BUF_SIZE]);
    let progress = discipline.receive(&data[progress.bytes_consumed..], 8, 8);
    assert_eq!(progress.bytes_consumed, 3);
    assert_eq!(consumer.pop_slice(&mut output), 3);
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
        let mut ldisc = TtyPort::new(
            terminal,
            TtyConfig {
                reader: MockReader::with_chunk_limit(&data, 7),
                writer: MockWriter::default(),
                process_mode: ProcessMode::Work(khal::percpu::this_cpu_id()),
            },
        );
        let (work, cpu_id) = ldisc.input_work().unwrap();
        let input_waker = ldisc.input_waker().unwrap();
        let weak = Arc::downgrade(&work);
        input_waker.wake_by_ref();
        let mut received = Vec::new();
        while received.len() < data.len() {
            await_input(&mut ldisc);
            let mut bytes = [0; 11];
            let count = ldisc.read(&mut bytes).unwrap();
            received.extend_from_slice(&bytes[..count]);
        }
        work.shutdown().unwrap();
        input_waker.wake_by_ref();
        assert_eq!(received, data);
        ldisc.drain_input();
        assert_eq!(work.queue_default(Some(cpu_id)), Err(SubmitError::Closed));
        assert!(!ldisc.poll_read());
        drop(ldisc);
        drop(work);
        assert!(weak.upgrade().is_none());
        input_waker.wake_by_ref();
    }
}

#[def_test]
fn input_and_pending_output_each_obey_the_budget() {
    let mut data = [b'a'; 600];
    data[255] = b'\n';
    let (mut ldisc, _) = new_manual_ldisc(&data, |_| {});
    let Processor::Manual(reader) = &mut ldisc.processor else {
        unreachable!()
    };
    let progress = reader.process_budget(256);
    assert_eq!(progress.bytes_consumed, 256);
    assert_eq!(progress.bytes_published, 0);
    assert_eq!(progress.stop, InputStop::BudgetExhausted);
    let progress = reader.process_budget(7);
    assert_eq!(progress.bytes_published, 7);
    assert_eq!(progress.stop, InputStop::BudgetExhausted);
    ldisc.drain_input();
    let Processor::Manual(reader) = &mut ldisc.processor else {
        unreachable!()
    };
    reader.process_budget(0);
    assert!(!ldisc.clear_line_buf.load(Ordering::Relaxed));
    assert_eq!(reader.process_budget(256).bytes_published, 0);
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
    let mut ldisc = TtyPort::new(
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
    let (work, _) = ldisc.input_work().unwrap();
    let input_waker = ldisc.input_waker().unwrap();
    input_waker.wake_by_ref();
    gate.wait_until(|| entered.load(Ordering::Acquire));
    input_waker.wake_by_ref();
    release.store(true, Ordering::Release);
    gate.notify_all(false);
    await_input(&mut ldisc);
    let mut bytes = [0; 8];
    let count = ldisc.read(&mut bytes).unwrap();
    work.shutdown().unwrap();
    assert_eq!(&bytes[..count], b"z\n");
}

#[def_test]
fn input_notifications_stay_with_their_source() {
    let make_port = |bytes: &[u8]| {
        TtyPort::new(
            Arc::default(),
            TtyConfig {
                reader: MockReader::new(bytes),
                writer: MockWriter::default(),
                process_mode: ProcessMode::Work(khal::percpu::this_cpu_id()),
            },
        )
    };
    let mut a = make_port(b"a\n");
    let mut b = make_port(b"b\n");
    let (work_a, _) = a.input_work().unwrap();
    let (work_b, _) = b.input_work().unwrap();
    let wake_a = a.input_waker().unwrap();
    let wake_b = b.input_waker().unwrap();
    wake_b.wake_by_ref();
    await_input(&mut b);
    assert!(!a.poll_read());
    let mut bytes = [0; 8];
    let count = b.read(&mut bytes).unwrap();
    assert_eq!(&bytes[..count], b"b\n");
    wake_a.wake_by_ref();
    await_input(&mut a);
    let count = a.read(&mut bytes).unwrap();
    assert_eq!(&bytes[..count], b"a\n");
    work_a.shutdown().unwrap();
    work_b.shutdown().unwrap();
}
