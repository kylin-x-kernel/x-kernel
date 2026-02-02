#![cfg_attr(not(test), no_std)]
#![doc = include_str!("../README.md")]

extern crate alloc;

mod auxv;
mod info;
mod user_stack;

pub use self::{auxv::*, info::*, user_stack::app_stack_region};
