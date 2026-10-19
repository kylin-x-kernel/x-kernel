// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Backend input transport and task-context processing for terminal ports.

use alloc::{
    boxed::Box,
    collections::VecDeque,
    sync::{Arc, Weak},
};
use core::{ops::Range, task::Waker};

use kcpu_id_map::LogicalCpuId;
use kerrno::{KError, KResult};
use kpoll::{PollContext, PollRegisterError, PollSet};
use kspin::SpinNoIrq;
use kworkng::{SubmitError, WorkHandle};
use ringbuf::{
    CachingCons,
    traits::{Consumer, Split},
};

use crate::terminal::{
    Terminal,
    ldisc::{InputProgress, InputStop, LineDiscipline, OutputLineDiscipline},
};

const BUF_SIZE: usize = 80;
const INPUT_WORK_BUDGET_BYTES: usize = 256;

pub(crate) type ReadBuf = Arc<ringbuf::StaticRb<u8, BUF_SIZE>>;

const INPUT_CAPACITY: usize = 4096;

struct IngressState {
    bytes: VecDeque<u8>,
    is_closed: bool,
}

/// Port-owned receive queue, analogous to a TTY flip buffer.
/// Shared with push sources; neither line-discipline state nor writer storage.
/// Coordinates raw readability, producer space and deferred input processing.
/// Queue locks never cover parsing, echo, signal delivery, or waiter callbacks.
pub struct InputIngress {
    state: SpinNoIrq<IngressState>,
    readable: Arc<PollSet>,
    space_available: PollSet,
    input_waker: SpinNoIrq<Option<Waker>>,
}

impl Default for InputIngress {
    fn default() -> Self {
        Self {
            state: SpinNoIrq::new(IngressState {
                bytes: VecDeque::with_capacity(INPUT_CAPACITY),
                is_closed: false,
            }),
            readable: Arc::new(PollSet::new()),
            space_available: PollSet::new(),
            input_waker: SpinNoIrq::new(None),
        }
    }
}

impl InputIngress {
    /// Accepts the fitting prefix without waiting; returns `Io` after closure.
    /// Nonempty writes with no room return `WouldBlock`, never discard bytes.
    pub fn push_input(&self, bytes: &[u8]) -> KResult<usize> {
        if bytes.is_empty() {
            return Ok(0);
        }
        let accepted = {
            let mut state = self.state.lock();
            if state.is_closed {
                return Err(KError::Io);
            }
            let accepted = bytes.len().min(INPUT_CAPACITY - state.bytes.len());
            state.bytes.extend(bytes[..accepted].iter().copied());
            accepted
        };
        if accepted == 0 {
            return Err(KError::WouldBlock);
        }
        let waker = self.input_waker.lock().clone();
        self.readable.wake();
        if let Some(waker) = waker {
            waker.wake();
        }
        Ok(accepted)
    }

    /// Reports byte capacity currently available to a producer.
    pub fn available_input_room(&self) -> usize {
        let state = self.state.lock();
        if state.is_closed {
            0
        } else {
            INPUT_CAPACITY - state.bytes.len()
        }
    }

    pub(crate) fn read(&self, buf: &mut [u8]) -> usize {
        let read = {
            let mut state = self.state.lock();
            let read = buf.len().min(state.bytes.len());
            for destination in &mut buf[..read] {
                if let Some(byte) = state.bytes.pop_front() {
                    *destination = byte;
                }
            }
            read
        };
        if read != 0 {
            self.space_available.wake();
        }
        read
    }

    pub(crate) fn clear(&self) {
        self.state.lock().bytes.clear();
        self.space_available.wake();
    }

    pub(crate) fn close(&self) {
        self.state.lock().is_closed = true;
        self.readable.wake();
        self.space_available.wake();
    }

    pub(crate) fn is_closed(&self) -> bool {
        self.state.lock().is_closed
    }

    fn is_readable(&self) -> bool {
        let state = self.state.lock();
        !state.bytes.is_empty() || state.is_closed
    }

    pub(crate) fn readable(&self) -> Arc<PollSet> {
        self.readable.clone()
    }

    pub(crate) fn register_space(
        &self,
        context: &mut PollContext<'_>,
    ) -> Result<(), PollRegisterError> {
        context.register(&self.space_available)
    }

    pub(crate) fn notify_space(&self) {
        self.space_available.wake();
    }
}

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
    /// Read ingress bytes directly without line-discipline processing.
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

    /// Shares receiver-owned ingress for push sources; pull adapters use `None`.
    fn input_ingress(&self) -> Option<Arc<InputIngress>> {
        None
    }
}
/// Output used by terminal writes and line-discipline echo in task context.
pub trait TtyWrite: Send + Sync + 'static {
    /// Returns the accepted prefix, or `WouldBlock` if no nonempty prefix fits.
    /// Console backends may explicitly provide best-effort delivery instead.
    /// With producers serialized, a nonempty buffer no longer than the preceding
    /// `write_room()` result must be accepted in full, or fail before publishing
    /// any bytes. Larger buffers may return a partial prefix. Transport closure
    /// may invalidate the room snapshot, but must not partially publish a unit.
    fn write(&self, buf: &[u8]) -> KResult<usize>;

    /// Reports backend-owned output processing, avoiding a second expansion.
    fn handles_output_processing(&self) -> bool {
        false
    }

    /// Reports available output bytes; unthrottled backends use the default.
    /// This is guaranteed acceptance capacity when producers are serialized and
    /// consumers only free space. Callers must exclude competing writes between
    /// this snapshot and `write`; returning `usize::MAX` promises full acceptance.
    fn write_room(&self) -> usize {
        usize::MAX
    }

    /// Registers output-space changes. Callers recheck after registration.
    fn register_tx(&self, _context: &mut PollContext<'_>) -> Result<(), PollRegisterError> {
        Ok(())
    }

    /// Wakes writers after a change to output processing requirements.
    fn notify_tx(&self) {}

    /// Discards output awaiting consumption by the peer receive queue and wakes
    /// space waiters. Backends without queued output retain the default no-op.
    fn flush_output(&self) {}

    /// Reports a closed transport so readiness can wake blocked writers.
    fn is_closed(&self) -> bool {
        false
    }
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
    pub(crate) fn shutdown(&self) {
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
    ingress: Arc<InputIngress>,
    discipline: LineDiscipline<W>,
    read_buf: [u8; BUF_SIZE],
    read_range: Range<usize>,
}

impl<R: TtyRead, W: TtyWrite> InputReader<R, W> {
    fn fill_ingress(&mut self) {
        if self.reader.input_ingress().is_none() {
            let count = self.ingress.available_input_room().min(BUF_SIZE);
            if count != 0 {
                let mut bytes = [0; BUF_SIZE];
                let read = self.reader.read(&mut bytes[..count]);
                // Pull adapters are the sole producer of their port ingress.
                let _ = self.ingress.push_input(&bytes[..read]);
            }
        }
    }

    fn clear_input(&mut self) {
        self.ingress.clear();
        self.read_range = 0..0;
        self.discipline.clear_input();
    }

    fn poll(&mut self) -> bool {
        self.process_batch(usize::MAX, usize::MAX).bytes_published > 0
    }

    fn process_batch(&mut self, input_budget: usize, output_budget: usize) -> InputProgress {
        if input_budget != 0 && output_budget != 0 && self.read_range.is_empty() {
            self.fill_ingress();
            let count = self.ingress.read(&mut self.read_buf);
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

enum Processor<R, W> {
    Manual(InputReader<R, W>),
    Work {
        work: Arc<InputWork>,
        poll_rx: Arc<PollSet>,
        input_waker: Waker,
        reader: Arc<ksync::Mutex<InputReader<R, W>>>,
    },
    None(R, Arc<PollSet>),
}

/// Connects a backend to terminal input processing and readable notifications.
///
/// The port owns queued raw input; processing retains only one prefetch
/// batch. Work bindings are host integration details, not line-discipline state.
pub struct TtyPort<R, W> {
    terminal: Arc<Terminal>,
    buf_rx: CachingCons<ReadBuf>,
    processor: Processor<R, W>,
    pub(crate) ingress: Arc<InputIngress>,
    pub(crate) output: Arc<ksync::Mutex<OutputLineDiscipline<W>>>,
}

impl<R: TtyRead, W: TtyWrite> TtyPort<R, W> {
    /// Creates input processing state without starting Work-mode callbacks.
    ///
    /// # Panics
    /// Work mode requires sleepable context and initialized normal workers.
    /// Allocation failure is fatal.
    pub fn new(terminal: Arc<Terminal>, config: TtyConfig<R, W>) -> Self {
        let (buf_tx, buf_rx) = ReadBuf::default().split();

        let ingress = config
            .reader
            .input_ingress()
            .unwrap_or_else(|| Arc::new(InputIngress::default()));
        let output = Arc::new(ksync::Mutex::new(OutputLineDiscipline::new(
            terminal.clone(),
            config.writer,
            matches!(&config.process_mode, ProcessMode::None(_)),
        )));
        let reader = InputReader {
            reader: config.reader,
            ingress: ingress.clone(),
            discipline: LineDiscipline::new(terminal.clone(), output.clone(), buf_tx),
            read_buf: [0; BUF_SIZE],
            read_range: 0..0,
        };

        let processor = match config.process_mode {
            ProcessMode::Manual => Processor::Manual(reader),
            ProcessMode::Work(cpu_id) => {
                let poll_rx = Arc::new(PollSet::new());
                let ready = poll_rx.clone();
                let reader = Arc::new(ksync::Mutex::new(reader));
                let worker_reader = reader.clone();
                let work: Arc<InputWork> = Arc::new_cyclic(|weak: &Weak<InputWork>| {
                    // Requeue must not keep the callback's owner alive forever.
                    let weak = weak.clone();
                    let callback_fn: Box<dyn Fn() + Send + Sync> = Box::new(move || {
                        let Some(work) = weak.upgrade() else { return };
                        if work.work.lock().is_none() {
                            return;
                        }
                        let progress = worker_reader.lock().process_budget(INPUT_WORK_BUDGET_BYTES);
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
                    reader,
                }
            }
            ProcessMode::None(poll_rx) => Processor::None(reader.reader, poll_rx),
        };
        if let Processor::Work { input_waker, .. } = &processor {
            *ingress.input_waker.lock() = Some(input_waker.clone());
        }
        Self {
            terminal,
            buf_rx,
            processor,
            ingress,
            output,
        }
    }

    /// Discards port-owned ingress, prefetch, canonical and cooked input.
    /// Serializes with work processing; hardware queues remain source-owned.
    pub fn drain_input(&mut self) {
        match &mut self.processor {
            Processor::Manual(reader) => {
                reader.clear_input();
                self.buf_rx.clear();
            }
            Processor::Work { reader, .. } => {
                let mut reader = reader.lock();
                reader.clear_input();
                self.buf_rx.clear();
            }
            Processor::None(..) => {
                self.ingress.clear();
                self.buf_rx.clear();
            }
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

    /// Tests terminal readability, driving pull adapters when necessary.
    /// Task context; manual processing may echo or deliver signals.
    pub fn poll_read(&mut self) -> bool {
        match &mut self.processor {
            Processor::Manual(reader) => {
                reader.poll();
            }
            Processor::None(reader, _) => {
                // Only pull-source adapters fill here; PTYs push directly.
                if reader.input_ingress().is_none() {
                    let room = self.ingress.available_input_room().min(BUF_SIZE);
                    if room != 0 {
                        let mut bytes = [0; BUF_SIZE];
                        let count = reader.read(&mut bytes[..room]);
                        let _ = self.ingress.push_input(&bytes[..count]);
                    }
                }
                return self.ingress.is_readable();
            }
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

    /// Finishes pending processing before consuming peer hangup.
    /// Task context only; the caller has observed permanent peer closure.
    /// Serializes with input work and returns published bytes without VMIN waits.
    pub(crate) fn read_after_peer_close(&mut self, buf: &mut [u8]) -> KResult<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        match &mut self.processor {
            Processor::Manual(reader) => {
                reader.process_budget(usize::MAX);
            }
            Processor::Work {
                reader, poll_rx, ..
            } => {
                let progress = reader.lock().process_budget(usize::MAX);
                if progress.bytes_published != 0 {
                    poll_rx.wake();
                }
            }
            Processor::None(..) => return self.read(buf),
        }
        let read = self.buf_rx.pop_slice(buf);
        if read == 0 { Err(KError::Io) } else { Ok(read) }
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
            self.poll_read();
            let read = self.ingress.read(buf);
            return if read == 0 {
                if self.ingress.is_closed() {
                    Ok(0)
                } else {
                    Err(KError::WouldBlock)
                }
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
        self.ingress.close();
        if let Processor::Work { work, .. } = &self.processor {
            work.close();
        }
    }
}

unittest::path_mod_test!(tests, "../ut-tests/port.rs");
