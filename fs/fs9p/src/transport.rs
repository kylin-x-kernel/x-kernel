//! Transport abstraction for 9P request/response traffic.

use alloc::string::String;

/// Transport for sending raw 9P requests and receiving replies.
pub trait Transport: Send + Sync {
    /// Send `req` and write the response into `resp`, returning the used length.
    fn request(&self, req: &[u8], resp: &mut [u8]) -> Result<usize, String>;
}
