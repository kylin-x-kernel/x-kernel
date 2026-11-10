// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! The card's synchronization vocabulary, modeled on Linux's dma-fence layer.
//!
//! The modules are named after the Linux sources they implement, and each one
//! owns exactly one concept:
//!
//! * [`dma_fence`] — a completion point; `dma_fence.c`;
//! * [`fence_list`] — an immutable set of fences published as one value;
//! * [`fence_array`] — a set merged into one completion point; `dma-fence-array.c`;
//! * [`dma_resv`] — implicit ordering of one buffer; `dma-resv.c`;
//! * [`sync_file`] — a fence exported as a file; `sync_file.c`;
//! * [`wait`] — the task-context wait on a fence set;
//! * [`fence_access`] — the user-facing fence syscall surface (sync_file create,
//!   merge, poll, fence array info).
//!
//! The card's own users — KMS commits, reservations of framebuffers — depend on
//! these types directly. Nothing above this layer refers to a raw fence fd or to
//! a kernel pointer: user fds are resolved through [`fence_access`] and kernel
//! waits go through [`wait`].

pub(crate) mod dma_fence;
pub(crate) mod dma_resv;
pub(crate) mod fence_access;
pub(crate) mod fence_array;
pub(crate) mod fence_chain;
pub(crate) mod fence_list;
pub(crate) mod sync_file;
pub(crate) mod syncobj_file;
pub(crate) mod wait;
pub(crate) mod wakeable;
