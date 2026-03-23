use core::mem::MaybeUninit;

#[repr(C, align(64))]
#[derive(Clone)]
pub struct EarlyBootArgs {
    pub args: [usize; 4],
    pub virt_entry: *mut (),
    pub kimage_addr_lma: *mut (),
    pub kimage_addr_vma: *mut (),
    pub stack_top_lma: *mut (),
    pub stack_top_vma: *mut (),
    pub kcode_end: *mut (),
    pub el: usize,
    pub kliner_offset: usize,
    pub page_size: usize,
    pub debug: usize,
}

impl EarlyBootArgs {
    pub const fn new() -> Self {
        unsafe { MaybeUninit::zeroed().assume_init() }
    }

    pub fn debug(&self) -> bool {
        self.debug > 0
    }
}

impl Default for EarlyBootArgs {
    fn default() -> Self {
        Self::new()
    }
}

#[unsafe(link_section = ".data")]
pub static mut BOOT_ARGS: EarlyBootArgs = EarlyBootArgs::new();
