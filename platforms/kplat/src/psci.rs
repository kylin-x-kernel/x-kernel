use kplat_macros::device_interface;

#[device_interface]
pub trait PsciOp {
    fn dma_share(pa: usize, size: usize);
    fn dma_unshare(pa: usize, size: usize);
}
