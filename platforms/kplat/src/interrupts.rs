pub use handler_table::HandlerTable;
use kplat_macros::device_interface;

pub type Handler = handler_table::Handler;

pub enum TargetCpu {
    Self_,
    Specific(usize),
    AllButSelf { me: usize, total: usize },
}

#[device_interface]
pub trait IntrManager {
    fn enable(id: usize, on: bool);
    fn reg_handler(id: usize, h: Handler) -> bool;
    fn unreg_handler(id: usize) -> Option<Handler>;
    fn dispatch_irq(id: usize) -> Option<usize>;

    fn notify_cpu(id: usize, target: TargetCpu);
    fn set_prio(id: usize, prio: u8);

    fn save_disable() -> usize;
    fn restore(flags: usize);

    fn enable_local();
    fn disable_local();
    fn is_enabled() -> bool;
}
