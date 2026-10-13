// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Internal task-reader accounting shared with the scheduler.
//!
//! This crate does not yet provide publication, grace periods or reclamation.
//! A recorded scope alone is not permission to free or dereference retired data.

#![no_std]

mod node;
mod reader;

pub use node::ReaderNode;
pub use reader::{ReadGuard, TaskReader};
