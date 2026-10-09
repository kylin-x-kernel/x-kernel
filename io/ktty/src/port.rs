// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Backend input transport and task-context processing for terminal ports.

use alloc::{
    boxed::Box,
    sync::{Arc, Weak},
};
use core::{
    ops::Range,
    sync::atomic::{AtomicBool, Ordering},
    task::Waker,
};

use kcpu_id_map::LogicalCpuId;
use kerrno::{KError, KResult};
use kpoll::{PollContext, PollRegisterError, PollSet};
use kspin::SpinNoIrq;
use kworkng::{SubmitError, WorkHandle};
use ringbuf::{
    CachingCons, CachingProd,
    traits::{Consumer, Producer, Split},
};

use crate::terminal::{
    Terminal,
    ldisc::{InputProgress, InputStop, LineDiscipline},
};

const BUF_SIZE: usize = 80;
const INPUT_WORK_BUDGET_BYTES: usize = 256;

pub(crate) type ReadBuf = Arc<ringbuf::StaticRb<u8, BUF_SIZE>>;

/// How should we process inputs?
pub enum ProcessMode {
    /// Process inputs only on call to `read`
    ///
    /// This is the fallback strategy and is rather limited. For instance, you
    /// can't interrupt a running program by Ctrl+C unless it's not blocked on a
    /// `read` call to the terminal, since the signal is emitted only when
    /// inputs are being processed.
    Manual,
    /// Processes input in finite task-context work rounds on a ready CPU.
    ///
    /// Construct in sleepable context after normal-worker startup.
    /// The owner must connect the
    /// input notification to its source and request an initial check. Dropping
    /// the port closes submissions; kworkng reclaims callback state asynchronously.
    Work(LogicalCpuId),
    /// Do not process inputs.
    ///
    /// This is only used by the master side of pseudo tty. The argument is the
    /// [`PollSet`] for incoming data.
    None(Arc<PollSet>),
}

/// Backend I/O and execution policy for one TTY input port.
pub struct TtyConfig<R, W> {
    /// Nonblocking source of raw input bytes.
    pub reader: R,
    /// Task-context output used for writes and echo.
    pub writer: W,
    /// How the port drives input processing.
    pub process_mode: ProcessMode,
}

/// Nonblocking backend input consumed by the port, not by the line discipline.
pub trait TtyRead: Send + Sync + 'static {
    /// Copies an available prefix into `buf`, returning at most `buf.len()`.
    /// Must not wait for more input. Called by the port in task context.
    fn read(&mut self, buf: &mut [u8]) -> usize;
}
/// Output used by terminal writes and line-discipline echo in task context.
pub trait TtyWrite: Send + Sync + 'static {
    /// Writes according to the backend's output policy. This existing contract
    /// does not report partial writes or output backpressure.
    fn write(&self, buf: &[u8]);
}

type InputWorkHandle = WorkHandle<Box<dyn Fn() + Send + Sync>>;

/// Owns the input submission gate independently of the sleeping reader lock.
pub(crate) struct InputWork {
    work: SpinNoIrq<Option<InputWorkHandle>>,
    cpu_id: LogicalCpuId,
}

impl InputWork {
    fn queue(&self) {
        let result = {
            let work = self.work.lock();
            let Some(work) = work.as_ref() else { return };
            work.queue_default(Some(self.cpu_id))
        };
        match result {
            Ok(_) | Err(SubmitError::AlreadyQueued | SubmitError::Closed) => {}
            error => warn!("TTY input work could not be queued: {error:?}"),
        }
    }

    /// Closes admission without waiting, including when called from an IRQ.
    /// An executing batch may finish; callback captures are reclaimed by kworkng.
    pub(crate) fn close(&self) {
        let work = self.work.lock().take();
        drop(work);
    }

    #[cfg(unittest)]
    #[expect(
        clippy::unwrap_used,
        reason = "CG-021-no-panic-on-reachable-failure: test scenario: shutdown failure fails the \
                  calling unittest"
    )]
    fn shutdown(&self) {
        let work = self.work.lock().take();
        if let Some(work) = work {
            work.shutdown().unwrap();
        }
    }
}

struct InputWake {
    work: Weak<InputWork>,
}

impl alloc::task::Wake for InputWake {
    fn wake(self: Arc<Self>) {
        alloc::task::Wake::wake_by_ref(&self);
    }

    fn wake_by_ref(self: &Arc<Self>) {
        if let Some(work) = self.work.upgrade() {
            work.queue();
        }
    }
}

/// Owns backend prefetch and the unconsumed suffix, not terminal policy.
struct InputReader<R, W> {
    reader: R,
    discipline: LineDiscipline<W>,
    read_buf: [u8; BUF_SIZE],
    read_range: Range<usize>,
}

impl<R: TtyRead, W: TtyWrite> InputReader<R, W> {
    fn poll(&mut self) -> bool {
        self.process_batch(usize::MAX, usize::MAX).bytes_published > 0
    }

    fn process_batch(&mut self, input_budget: usize, output_budget: usize) -> InputProgress {
        if input_budget != 0 && output_budget != 0 && self.read_range.is_empty() {
            let count = self.reader.read(&mut self.read_buf);
            self.read_range = 0..count;
        }
        let progress = self.discipline.receive(
            &self.read_buf[self.read_range.clone()],
            input_budget,
            output_budget,
        );
        self.read_range.start += progress.bytes_consumed;
        progress
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
}

struct SimpleReader<R> {
    reader: R,
    read_buf: [u8; BUF_SIZE],
    buf_tx: CachingProd<ReadBuf>,
}
impl<R: TtyRead> SimpleReader<R> {
    pub(crate) fn poll(&mut self) {
        let read = self.reader.read(&mut self.read_buf);
        for ch in &self.read_buf[..read] {
            if *ch == b'\n' {
                // Echo is best effort; bytes are dropped when the transmit queue is full.
                let _ = self.buf_tx.try_push(b'\r');
            }
            let _ = self.buf_tx.try_push(*ch);
        }
    }
}

enum Processor<R, W> {
    Manual(InputReader<R, W>),
    Work {
        work: Arc<InputWork>,
        poll_rx: Arc<PollSet>,
        input_waker: Waker,
    },
    None(SimpleReader<R>, Arc<PollSet>),
}

/// Connects a backend to terminal input processing and readable notifications.
///
/// Backend storage owns queued raw input; this port retains only one prefetch
/// batch. Work bindings are host integration details, not line-discipline state.
pub struct TtyPort<R, W> {
    terminal: Arc<Terminal>,
    buf_rx: CachingCons<ReadBuf>,
    clear_line_buf: Arc<AtomicBool>,
    processor: Processor<R, W>,
}

impl<R: TtyRead, W: TtyWrite> TtyPort<R, W> {
    /// Creates input processing state without starting Work-mode callbacks.
    ///
    /// # Panics
    /// Work mode requires sleepable context and initialized normal workers.
    /// Allocation failure is fatal.
    pub fn new(terminal: Arc<Terminal>, config: TtyConfig<R, W>) -> Self {
        let (buf_tx, buf_rx) = ReadBuf::default().split();

        let clear_line_buf = Arc::new(AtomicBool::new(false));
        let reader = InputReader {
            reader: config.reader,
            discipline: LineDiscipline::new(
                terminal.clone(),
                config.writer,
                buf_tx,
                clear_line_buf.clone(),
            ),
            read_buf: [0; BUF_SIZE],
            read_range: 0..0,
        };

        let processor = match config.process_mode {
            ProcessMode::Manual => Processor::Manual(reader),
            ProcessMode::Work(cpu_id) => {
                let poll_rx = Arc::new(PollSet::new());
                let ready = poll_rx.clone();
                let reader = ksync::Mutex::new(reader);
                let work: Arc<InputWork> = Arc::new_cyclic(|weak: &Weak<InputWork>| {
                    // Requeue must not keep the callback's owner alive forever.
                    let weak = weak.clone();
                    let callback_fn: Box<dyn Fn() + Send + Sync> = Box::new(move || {
                        let Some(work) = weak.upgrade() else { return };
                        if work.work.lock().is_none() {
                            return;
                        }
                        let progress = reader.lock().process_budget(INPUT_WORK_BUDGET_BYTES);
                        if progress.bytes_published > 0 {
                            ready.wake();
                        }
                        if progress.stop == InputStop::BudgetExhausted {
                            work.queue();
                        }
                    });
                    #[expect(
                        clippy::expect_used,
                        reason = "CG-021-no-panic-on-reachable-failure: TTY work mode is \
                                  constructed in sleepable task context after the permanent \
                                  normal worker service is installed"
                    )]
                    let work =
                        WorkHandle::new(callback_fn).expect("TTY input requires normal workers");
                    InputWork {
                        work: SpinNoIrq::new(Some(work)),
                        cpu_id,
                    }
                });
                Processor::Work {
                    input_waker: Waker::from(Arc::new(InputWake {
                        work: Arc::downgrade(&work),
                    })),
                    work,
                    poll_rx,
                }
            }
            ProcessMode::None(poll_rx) => {
                // Destruct the reader here
                Processor::None(
                    SimpleReader {
                        reader: reader.reader,
                        read_buf: [0; BUF_SIZE],
                        buf_tx: reader.discipline.into_output(),
                    },
                    poll_rx,
                )
            }
        };
        Self {
            terminal,
            buf_rx,
            clear_line_buf,
            processor,
        }
    }

    /// Discards cooked input and requests clearing the pending canonical line.
    /// Existing backend queues and the prefetch batch are not flushed.
    pub fn drain_input(&mut self) {
        self.buf_rx.clear();
        self.clear_line_buf.store(true, Ordering::Relaxed);
        if let Processor::Work { work, .. } = &self.processor {
            work.queue();
        }
    }

    /// Returns a prepared, IRQ-safe notification to recheck the input source.
    /// It neither processes input inline nor keeps work alive. After shutdown,
    /// notifications are ignored. This is not terminal-readable notification.
    pub(crate) fn input_waker(&self) -> Option<Waker> {
        match &self.processor {
            Processor::Work { input_waker, .. } => Some(input_waker.clone()),
            _ => None,
        }
    }

    /// Shares the submission owner so PTY retirement need not lock the reader.
    pub(crate) fn input_work(&self) -> Option<Arc<InputWork>> {
        match &self.processor {
            Processor::Work { work, .. } => Some(work.clone()),
            _ => None,
        }
    }

    /// Tests terminal readability, driving manual/bypass input when necessary.
    /// Task context; manual processing may echo or deliver signals.
    pub fn poll_read(&mut self) -> bool {
        match &mut self.processor {
            Processor::Manual(reader) => {
                reader.poll();
            }
            Processor::None(reader, _) => reader.poll(),
            _ => {}
        }
        crate::terminal::ldisc::read_ready(&self.terminal, &self.buf_rx)
    }

    /// Registers input readiness; callers must recheck readiness after registering.
    /// Returns the poll source's registration error if it cannot retain the wait.
    pub fn register_rx(&self, context: &mut PollContext<'_>) -> Result<(), PollRegisterError> {
        match &self.processor {
            Processor::Manual(_) => {
                // Manual mode has no external wake source, so force an immediate
                // recheck by waking the current waiter.
                context.wake_by_ref();
            }
            Processor::None(_, set) | Processor::Work { poll_rx: set, .. } => {
                context.register(set)?;
            }
        }
        Ok(())
    }

    /// Reads processed input without waiting and restarts a blocked producer.
    /// Returns `WouldBlock` when the current terminal read policy is not met.
    /// Nonzero VTIME retains the single-byte approximation; inter-byte timing
    /// is not implemented.
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

        let read = crate::terminal::ldisc::read_processed(&self.terminal, &mut self.buf_rx, buf)?;
        if let Processor::Work { work, .. } = &self.processor {
            work.queue();
        }
        Ok(read)
    }
}

impl<R, W> Drop for TtyPort<R, W> {
    fn drop(&mut self) {
        if let Processor::Work { work, .. } = &self.processor {
            work.close();
        }
    }
}

#[unittest::mod_test]
mod tests {
    include!("tests/port.rs");
}
