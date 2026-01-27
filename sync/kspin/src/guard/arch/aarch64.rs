#[inline]
pub fn save_disable() -> usize {
    crate_interface::call_interface!(crate::guard::KernelGuardIf::save_disable)
}

#[inline]
pub fn restore(flags: usize) {
    crate_interface::call_interface!(crate::guard::KernelGuardIf::restore(flags))
}
