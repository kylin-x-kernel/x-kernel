//! I/O multiplexing syscalls.
//!
//! This module implements synchronous I/O multiplexing mechanisms including:
//! - select: Traditional file descriptor multiplexing
//! - poll: Enhanced multiplexing with better scalability
//! - epoll: High-performance event notification mechanism
//!
//! Allows monitoring multiple file descriptors for I/O events.

mod epoll;
mod poll;
mod select;

use alloc::{sync::Arc, vec::Vec};
use core::task::Context;

use kpoll::{IoEvents, Pollable};

pub use self::{epoll::*, poll::*, select::*};
use crate::file::FileLike;

struct FdPollSet(pub Vec<(Arc<dyn FileLike>, IoEvents)>);
impl Pollable for FdPollSet {
    fn poll(&self) -> IoEvents {
        unreachable!()
    }

    fn register(&self, context: &mut Context<'_>, _events: IoEvents) {
        for (file, events) in &self.0 {
            file.register(context, *events);
        }
    }
}
