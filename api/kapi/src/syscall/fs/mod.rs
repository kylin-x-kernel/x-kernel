//! File system related syscalls.
//!
//! This module implements various file system operations including:
//! - File I/O (read, write, seek, etc.)
//! - Directory operations (mkdir, rmdir, chdir, etc.)
//! - File descriptor operations (open, close, dup, etc.)
//! - File metadata and statistics (stat, fstat, etc.)
//! - File control (ioctl, fcntl, etc.)
//! - Special files (pipes, fifos, device files, etc.)

mod ctl;
mod event;
mod fd_ops;
mod io;
mod memfd;
mod mount;
mod pidfd;
mod pipe;
mod signalfd;
mod stat;

pub use self::{
    ctl::*, event::*, fd_ops::*, io::*, memfd::*, mount::*, pidfd::*, pipe::*, signalfd::*, stat::*,
};
