// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Shared kernel identity number-space allocation.

#![cfg_attr(not(test), no_std)]

extern crate alloc;

use alloc::{sync::Arc, vec::Vec};
use core::sync::atomic::{AtomicU32, Ordering};

use kerrno::{KError, KResult};
use lazyinit::LazyInit;

/// PID namespace identity.
#[derive(Debug)]
pub struct PidNamespace {
    parent: Option<Arc<PidNamespace>>,
    level: u32,
    next_nr: AtomicU32,
}

impl PidNamespace {
    /// Creates the root PID namespace.
    pub fn new_root() -> Self {
        Self {
            parent: None,
            level: 0,
            next_nr: AtomicU32::new(1),
        }
    }

    /// Creates a child PID namespace below `parent`.
    pub fn new_child(parent: &Arc<Self>) -> Arc<Self> {
        Arc::new(Self {
            parent: Some(parent.clone()),
            level: parent.level + 1,
            next_nr: AtomicU32::new(1),
        })
    }

    /// Returns the parent namespace, if any.
    pub fn parent(&self) -> Option<&Arc<PidNamespace>> {
        self.parent.as_ref()
    }

    /// Returns the namespace nesting level.
    pub fn level(&self) -> u32 {
        self.level
    }

    /// Returns `true` if this is the root PID namespace.
    pub fn is_root(&self) -> bool {
        self.parent.is_none()
    }

    fn allocate_nr(&self) -> KResult<u32> {
        self.next_nr
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |next| {
                next.checked_add(1)
            })
            .map_err(|_overflowed| KError::WouldBlock)
    }
}

/// A namespace-scoped task number.
#[derive(Debug, Clone)]
pub struct Upid {
    nr: u32,
    ns: Arc<PidNamespace>,
}

impl Upid {
    /// Returns the number within its namespace.
    pub fn nr(&self) -> u32 {
        self.nr
    }

    /// Returns the owning namespace.
    pub fn ns(&self) -> &Arc<PidNamespace> {
        &self.ns
    }
}

/// Shared task-number handle, analogous to Linux `struct pid`.
#[derive(Debug)]
pub struct PidHandle {
    numbers: Vec<Upid>,
}

impl PidHandle {
    /// Allocates a new handle visible in `active_ns` and all its ancestors.
    pub fn allocate_in(active_ns: &Arc<PidNamespace>) -> KResult<Arc<Self>> {
        let mut chain = Vec::new();
        let mut cursor = Some(active_ns.clone());
        while let Some(ns) = cursor {
            chain.push(ns.clone());
            cursor = ns.parent().cloned();
        }

        let mut numbers = Vec::with_capacity(chain.len());
        for ns in chain {
            numbers.push(Upid {
                nr: ns.allocate_nr()?,
                ns,
            });
        }

        Ok(Arc::new(Self { numbers }))
    }

    /// Creates a root-only handle with a fixed number.
    pub fn fixed_root(nr: u32) -> Arc<Self> {
        Arc::new(Self {
            numbers: alloc::vec![Upid {
                nr,
                ns: root_pid_namespace().clone(),
            }],
        })
    }

    /// Returns the root-visible number.
    pub fn root_nr(&self) -> u32 {
        self.numbers
            .iter()
            .find(|upid| upid.ns.is_root())
            .expect("pid handle must always have a root-visible number")
            .nr
    }

    /// Returns the number visible in `ns`, if present.
    pub fn nr_in(&self, ns: &Arc<PidNamespace>) -> Option<u32> {
        self.numbers
            .iter()
            .find(|upid| Arc::ptr_eq(upid.ns(), ns))
            .map(Upid::nr)
            .or_else(|| ns.is_root().then(|| self.root_nr()))
    }

    /// Returns the namespace-number vector.
    pub fn numbers(&self) -> &[Upid] {
        &self.numbers
    }
}

static ROOT_PID_NS: LazyInit<Arc<PidNamespace>> = LazyInit::new();

/// Returns the shared root PID namespace.
pub fn root_pid_namespace() -> &'static Arc<PidNamespace> {
    ROOT_PID_NS.call_once(|| Arc::new(PidNamespace::new_root()));
    ROOT_PID_NS.get().unwrap()
}

/// Allocates a root-visible PID/TID handle.
pub fn allocate_root_pid_handle() -> KResult<Arc<PidHandle>> {
    PidHandle::allocate_in(root_pid_namespace())
}

#[cfg(unittest)]
mod tests {
    use alloc::sync::Arc;

    use unittest::{assert, assert_eq, def_test};

    use super::{PidHandle, PidNamespace, root_pid_namespace};

    #[def_test]
    fn root_pid_handle_starts_at_one() {
        let ns = Arc::new(PidNamespace::new_root());
        let first = PidHandle::allocate_in(&ns).unwrap();
        let second = PidHandle::allocate_in(&ns).unwrap();
        assert_eq!(first.root_nr(), 1);
        assert_eq!(second.root_nr(), 2);
    }

    #[def_test]
    fn child_pid_namespace_gets_pid_one() {
        let root = root_pid_namespace().clone();
        let _root_task = PidHandle::allocate_in(&root).unwrap();
        let child = PidNamespace::new_child(&root);
        let handle = PidHandle::allocate_in(&child).unwrap();

        assert_eq!(handle.nr_in(&child), Some(1));
        assert!(handle.root_nr() >= 2);
    }

    #[def_test]
    fn fixed_root_handle_is_visible_to_any_root_namespace() {
        let other_root = Arc::new(PidNamespace::new_root());
        let handle = PidHandle::fixed_root(42);
        assert_eq!(handle.nr_in(&other_root), Some(42));
    }
}
