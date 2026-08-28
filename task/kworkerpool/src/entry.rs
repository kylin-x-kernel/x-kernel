// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

/// Opaque source identity carried by worker-pool entries.
///
/// The source is returned to the execution runtime when a worker claims an
/// entry. The pool stores it but never uses it for scheduling decisions.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct EntrySource(usize);

impl EntrySource {
    /// Creates a source identity from an integration-defined handle value.
    pub const fn new(value: usize) -> Self {
        Self(value)
    }

    /// Returns the integration-defined handle value.
    pub const fn as_usize(self) -> usize {
        self.0
    }
}

/// Opaque owner key used by worker-pool entries.
///
/// The owner groups entries for deferred-to-runnable promotion. It does not
/// encode workqueue identity inside the worker-pool core.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct EntryOwner(usize);

impl EntryOwner {
    /// Creates an owner key from an integration-defined handle value.
    pub const fn new(value: usize) -> Self {
        Self(value)
    }

    /// Returns the integration-defined handle value.
    pub const fn as_usize(self) -> usize {
        self.0
    }
}

/// Opaque comparable key used by worker-pool entries.
///
/// Runtime users compare this key against their own external work state when
/// claiming a runnable entry.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct EntryKey(usize);

impl EntryKey {
    /// Creates an entry key from an integration-defined handle value.
    pub const fn new(value: usize) -> Self {
        Self(value)
    }

    /// Returns the integration-defined handle value.
    pub const fn as_usize(self) -> usize {
        self.0
    }
}

/// Opaque payload handle used by worker-pool entries.
///
/// The worker-pool core returns this handle to the execution runtime after a
/// runnable entry is claimed; the actual work object remains owned outside
/// this crate.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct EntryPayload(usize);

impl EntryPayload {
    /// Creates a payload handle from an integration-defined value.
    pub const fn new(value: usize) -> Self {
        Self(value)
    }

    /// Returns the integration-defined handle value.
    pub const fn as_usize(self) -> usize {
        self.0
    }
}
