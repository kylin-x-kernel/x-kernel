//! Platform interrupt controller interface.

pub use handler_table::HandlerTable;
use kplat_macros::device_interface;

/// IRQ handler type.
pub type Handler = handler_table::Handler;

/// Target CPU(s) for inter-processor interrupts.
pub enum TargetCpu {
    /// Target the current CPU.
    Self_,
    /// Target a specific CPU by ID.
    Specific(usize),
    /// Target all CPUs except the caller.
    AllButSelf { me: usize, total: usize },
}

#[device_interface]
pub trait IntrManager {
    /// Enables or disables the given interrupt.
    fn enable(id: usize, on: bool);
    /// Registers a handler for the given interrupt.
    fn reg_handler(id: usize, h: Handler) -> bool;
    /// Unregisters the handler for the given interrupt.
    fn unreg_handler(id: usize) -> Option<Handler>;
    /// Dispatches a hardware IRQ and returns a logical IRQ number if any.
    fn dispatch_irq(id: usize) -> Option<usize>;

    /// Sends an IPI or interrupt notification to a target CPU.
    fn notify_cpu(id: usize, target: TargetCpu);
    /// Sets the priority for the given interrupt.
    fn set_prio(id: usize, prio: u8);

    /// Saves and disables local interrupt state.
    fn save_disable() -> usize;
    /// Restores local interrupt state saved by `save_disable`.
    fn restore(flags: usize);

    /// Enables local interrupts on the current CPU.
    fn enable_local();
    /// Disables local interrupts on the current CPU.
    fn disable_local();
    /// Returns whether local interrupts are enabled.
    fn is_enabled() -> bool;
}
