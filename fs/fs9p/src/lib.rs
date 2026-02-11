#![no_std]

//! Lightweight 9P client library for no_std targets.

extern crate alloc;

mod message;
mod parse;
mod protocol;
mod session;
mod transport;

pub use session::{FileAttr, P9DirEntry, P9Session as Session};
pub use transport::Transport;
