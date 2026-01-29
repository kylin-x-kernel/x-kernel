use kplat_macros::device_interface;

#[derive(Clone, Copy, Debug)]
pub enum NmiType {
    /// True hardware NMI (cannot be masked by IRQ disable)
    TrueNmi,
    /// Pseudo NMI (implemented via high-priority IRQ / FIQ / SGI)
    PseudoNmi,
    /// Not supported
    None,
}

pub type NmiHandler = fn();

#[device_interface]
pub trait NmiDef {
    fn init(thresh: u64) -> bool;
    fn nmi_type() -> NmiType;
    fn enable();
    fn disable();
    fn is_enabled() -> bool;
    fn name() -> &'static str;
    fn register_nmi_handler(cb: NmiHandler) -> bool;
}
