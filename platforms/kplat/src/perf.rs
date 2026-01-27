use kplat_macros::device_interface;

pub type PerfCb = fn();

#[device_interface]
pub trait PerfMgr {
    fn on_overflow() -> bool;
    fn reg_cb(idx: u32, cb: PerfCb) -> bool;
}
