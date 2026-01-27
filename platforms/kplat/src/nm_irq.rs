use kplat_macros::device_interface;

#[derive(Clone, Copy, Debug)]
pub enum NmiKind {
    Hw,
    Fake,
    None,
}

pub type NmiCallback = fn();

#[device_interface]
pub trait NmiDef {
    fn setup(thresh: u64) -> bool;
    fn cap() -> NmiKind;
    fn start();
    fn stop();
    fn running() -> bool;
    fn name() -> &'static str;
    fn reg(cb: NmiCallback) -> bool;
}
