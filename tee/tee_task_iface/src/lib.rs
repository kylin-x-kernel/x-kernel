#![no_std]

extern crate alloc;

use core::any::Any;

pub mod ta_ctx;

pub use ta_ctx::{SessionIdentity, TeeTaCtx};

/// Tee session context trait.
///
/// Stored behind `dyn` and used for type-erased access, so it must provide
/// downcasting via `as_any`.
pub trait TeeSessionCtxTrait {
    /// Get the any reference of the tee session context.
    fn as_any(&self) -> &dyn Any;
    /// Get the any mutable reference of the tee session context.
    fn as_any_mut(&mut self) -> &mut dyn Any;
}
