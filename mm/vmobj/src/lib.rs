// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Object-neutral VM object and reverse-mapping language.
//!
//! This crate holds the common vocabulary shared by file-backed and anonymous
//! VM objects:
//!
//! - stable object identities;
//! - object-side mapped-view records;
//! - view-hit invalidation work emitted by object owners;
//! - neutral invalidate requests consumed by address spaces.
//!
//! Linux references:
//! - `struct address_space` / `i_mmap` for file-backed objects
//! - `anon_vma` / rmap for anonymous objects
#![no_std]

extern crate alloc;

use alloc::{sync::Arc, vec::Vec};
use core::sync::atomic::{AtomicU64, Ordering};

use memaddr::PAGE_SIZE_4K;

/// Stable identity for one file-backed VM object family.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct FileObjectId(u64);

impl FileObjectId {
    /// Returns the raw identity value.
    pub const fn raw(self) -> u64 {
        self.0
    }

    /// Creates a file object id from a raw integer.
    pub const fn from_raw(raw: u64) -> Self {
        Self(raw)
    }
}

/// Stable identity for one anonymous VM object family.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct AnonObjectId(u64);

impl AnonObjectId {
    /// Returns the raw identity value.
    pub const fn raw(self) -> u64 {
        self.0
    }

    /// Creates an anonymous object id from a raw integer.
    pub const fn from_raw(raw: u64) -> Self {
        Self(raw)
    }
}

/// Stable identifier for one backing object.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum VmObjectId {
    /// File-backed object owner, analogous to `address_space`.
    File(FileObjectId),
    /// Anonymous/private object owner, analogous to `anon_vma` families.
    Anon(AnonObjectId),
}

/// Stable identity for one object-side mapped view.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct MappingViewId(u64);

impl MappingViewId {
    /// Returns the raw identity value.
    pub const fn raw(self) -> u64 {
        self.0
    }

    /// Creates an identity from a raw integer.
    pub const fn from_raw(raw: u64) -> Self {
        Self(raw)
    }
}

static NEXT_MAPPING_VIEW_ID: AtomicU64 = AtomicU64::new(1);

/// Allocates a fresh stable object-side mapped-view identity.
pub fn next_mapping_view_id() -> MappingViewId {
    MappingViewId::from_raw(NEXT_MAPPING_VIEW_ID.fetch_add(1, Ordering::Relaxed))
}

/// High-level mapping mode registered against a backing object.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MappingViewKind {
    /// `MAP_SHARED` or equivalent shared file-backed mapping.
    Shared,
    /// `MAP_PRIVATE` or equivalent private file-backed mapping.
    Private,
}

/// One mapped VMA range as seen from a backing object.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MappingViewRange {
    /// VMA start address.
    pub vma_start: u64,
    /// VMA byte length.
    pub vma_len: usize,
    /// Object byte offset where this view starts.
    pub object_start: u64,
    /// Object byte length covered by this view.
    pub object_len: usize,
}

impl MappingViewRange {
    /// Returns whether both coordinate ranges are non-empty and non-overflowing.
    pub const fn is_valid(self) -> bool {
        self.vma_len > 0
            && self.object_len > 0
            && self.vma_start <= u64::MAX - self.vma_len as u64
            && self.object_start <= u64::MAX - self.object_len as u64
    }

    /// Returns the VMA exclusive end address.
    pub const fn vma_end(self) -> u64 {
        self.vma_start.saturating_add(self.vma_len as u64)
    }

    /// Returns the object exclusive end offset.
    pub const fn object_end(self) -> u64 {
        self.object_start.saturating_add(self.object_len as u64)
    }

    /// Returns whether this view range fully contains an object-side range.
    pub const fn contains_object_range(self, object_start: u64, object_len: usize) -> bool {
        object_len > 0
            && object_start <= u64::MAX - object_len as u64
            && object_start >= self.object_start
            && object_start + object_len as u64 <= self.object_end()
    }
}

/// Formal object-side record of one mapped VMA view.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MappingView {
    id: MappingViewId,
    mm_id: u64,
    range: MappingViewRange,
    kind: MappingViewKind,
}

impl MappingView {
    /// Creates a new object-side mapped view record if its range is valid.
    pub const fn try_new(
        id: MappingViewId,
        mm_id: u64,
        range: MappingViewRange,
        kind: MappingViewKind,
    ) -> Option<Self> {
        if !range.is_valid() {
            return None;
        }
        Some(Self {
            id,
            mm_id,
            range,
            kind,
        })
    }

    /// Creates a new object-side mapped view record.
    ///
    /// # Panics
    ///
    /// Panics if the range is invalid. Use [`Self::try_new`] for unvalidated ranges.
    pub const fn new(
        id: MappingViewId,
        mm_id: u64,
        range: MappingViewRange,
        kind: MappingViewKind,
    ) -> Self {
        #[expect(
            clippy::expect_used,
            reason = "CG-021-no-panic-on-reachable-failure: The infallible mapping-view \
                      constructor requires a valid range; unvalidated inputs use try_new"
        )]
        Self::try_new(id, mm_id, range, kind).expect("mapping view range must be valid")
    }

    /// Returns the stable registration id.
    pub const fn id(&self) -> MappingViewId {
        self.id
    }

    /// Returns the owning address-space identity.
    pub const fn mm_id(&self) -> u64 {
        self.mm_id
    }

    /// Returns the VMA start address.
    pub const fn vma_start(&self) -> u64 {
        self.range.vma_start
    }

    /// Returns the VMA exclusive end address.
    pub const fn vma_end(&self) -> u64 {
        self.range.vma_end()
    }

    /// Returns the VMA byte length.
    pub const fn vma_len(&self) -> usize {
        self.range.vma_len
    }

    /// Returns whether the VMA length is zero.
    pub const fn is_empty(&self) -> bool {
        self.range.vma_len == 0
    }

    /// Returns the object byte offset where this view starts.
    pub const fn object_start(&self) -> u64 {
        self.range.object_start
    }

    /// Returns the object exclusive end offset.
    pub const fn object_end(&self) -> u64 {
        self.range.object_end()
    }

    /// Returns the object byte length covered by this view.
    pub const fn object_len(&self) -> usize {
        self.range.object_len
    }

    /// Returns the full view range.
    pub const fn range(&self) -> MappingViewRange {
        self.range
    }

    /// Returns whether the VMA is shared or private.
    pub const fn kind(&self) -> MappingViewKind {
        self.kind
    }

    /// Returns the page offset (`vm_pgoff`) derived from the object byte start.
    pub const fn page_offset(&self) -> u64 {
        self.range.object_start / PAGE_SIZE_4K as u64
    }

    /// Returns whether this view overlaps the given object range.
    pub const fn overlaps_object_range(&self, start: u64, end: u64) -> bool {
        self.object_start() < end && self.object_end() > start
    }

    /// Returns whether this view fully contains an object-side range.
    pub const fn contains_object_range(&self, object_start: u64, object_len: usize) -> bool {
        self.range.contains_object_range(object_start, object_len)
    }

    /// Converts an object byte offset back into a VMA-relative byte offset.
    pub const fn object_to_vma_offset(&self, object_offset: u64) -> Option<u64> {
        if object_offset < self.object_start() || object_offset >= self.object_end() {
            return None;
        }
        Some(object_offset - self.object_start())
    }

    /// Maps one object-side page or byte-range event back into this view.
    pub fn page_hit(&self, object_start: u64, object_len: usize) -> Option<ObjectViewHit> {
        let object_end = object_start.checked_add(object_len as u64)?;
        if !self.overlaps_object_range(object_start, object_end) {
            return None;
        }
        let hit_start = object_start.max(self.object_start());
        let hit_end = object_end.min(self.object_end());
        Some(ObjectViewHit::new(
            self.clone(),
            hit_start,
            (hit_end - hit_start) as usize,
        ))
    }
}

/// Parameters for registering one VMA view against a backing object.
#[derive(Clone)]
pub struct MappingViewSpec {
    /// Owning address-space identity.
    pub mm_id: u64,
    /// VMA start address.
    pub vma_start: u64,
    /// VMA byte length.
    pub vma_len: usize,
    /// Starting byte offset covered by this view in the backing object.
    pub object_start: u64,
    /// Byte length covered by this view in the backing object.
    pub object_len: usize,
    /// Shared/private file mapping mode.
    pub kind: MappingViewKind,
    /// Optional reverse-mapping callback for this VMA view.
    pub notifier: Option<Arc<dyn MappingViewNotifier>>,
}

/// One object-side invalidation hit against a registered view.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObjectViewHit {
    view: MappingView,
    object_start: u64,
    object_len: usize,
}

impl ObjectViewHit {
    /// Creates one object-side invalidation hit if it is fully covered by the
    /// registered view.
    pub fn try_new(view: MappingView, object_start: u64, object_len: usize) -> Option<Self> {
        view.contains_object_range(object_start, object_len)
            .then_some(Self {
                view,
                object_start,
                object_len,
            })
    }

    /// Returns a suffix of this hit starting at the next aligned object offset.
    pub fn aligned_object_suffix(&self, alignment: u64) -> Option<Self> {
        if alignment == 0 {
            return None;
        }
        let remainder = self.object_start % alignment;
        let aligned_start = if remainder == 0 {
            self.object_start
        } else {
            self.object_start.checked_add(alignment - remainder)?
        };
        if aligned_start >= self.object_end() {
            return None;
        }
        Self::try_new(
            self.view.clone(),
            aligned_start,
            (self.object_end() - aligned_start) as usize,
        )
    }

    /// Creates one object-side invalidation hit.
    pub const fn new(view: MappingView, object_start: u64, object_len: usize) -> Self {
        assert!(
            view.contains_object_range(object_start, object_len),
            "object hit must stay within mapping view"
        );
        Self {
            view,
            object_start,
            object_len,
        }
    }

    /// Returns the registered view that was hit.
    pub const fn view(&self) -> &MappingView {
        &self.view
    }

    /// Returns the invalidated object byte start within the backing object.
    pub const fn object_start(&self) -> u64 {
        self.object_start
    }

    /// Returns the invalidated object byte length for this view.
    pub const fn object_len(&self) -> usize {
        self.object_len
    }

    /// Returns the invalidated object exclusive end offset.
    pub const fn object_end(&self) -> u64 {
        self.object_start.saturating_add(self.object_len as u64)
    }

    /// Returns the VMA start covered by this invalidation hit.
    pub fn vma_start(&self) -> u64 {
        #[expect(
            clippy::expect_used,
            reason = "CG-021-no-panic-on-reachable-failure: ObjectViewHit construction checks \
                      that the object hit lies inside its mapped view"
        )]
        let offset = self
            .view
            .object_to_vma_offset(self.object_start)
            .expect("object hit must stay within view");
        self.view.vma_start().saturating_add(offset)
    }

    /// Returns the VMA byte length covered by this invalidation hit.
    pub const fn vma_len(&self) -> usize {
        self.object_len
    }

    /// Returns the VMA exclusive end address for this invalidation hit.
    pub fn vma_end(&self) -> u64 {
        self.vma_start().saturating_add(self.vma_len() as u64)
    }

    /// Returns the hit range mapped back into VMA coordinates.
    pub fn vma_range(&self) -> MappingViewRange {
        MappingViewRange {
            vma_start: self.vma_start(),
            vma_len: self.vma_len(),
            object_start: self.object_start(),
            object_len: self.object_len(),
        }
    }
}

/// Object-side invalidation work emitted by one backing object operation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObjectInvalidateWork {
    object: VmObjectId,
    object_start: u64,
    object_len: usize,
    hits: Vec<ObjectViewHit>,
}

impl ObjectInvalidateWork {
    /// Creates one object-side invalidation work item if the work range is
    /// valid and covers every carried view hit.
    pub fn try_new(
        object: VmObjectId,
        object_start: u64,
        object_len: usize,
        hits: Vec<ObjectViewHit>,
    ) -> Option<Self> {
        if object_len == 0 || object_start > u64::MAX - object_len as u64 {
            return None;
        }
        for hit in &hits {
            if object_start > hit.object_start()
                || hit.object_end() > object_start + object_len as u64
            {
                return None;
            }
        }
        Some(Self {
            object,
            object_start,
            object_len,
            hits,
        })
    }

    /// Creates one object-side invalidation work item.
    ///
    /// # Panics
    ///
    /// Panics if the range is invalid or does not cover every hit.
    /// Use [`Self::try_new`] for unvalidated ranges and hits.
    pub fn new(
        object: VmObjectId,
        object_start: u64,
        object_len: usize,
        hits: Vec<ObjectViewHit>,
    ) -> Self {
        #[expect(
            clippy::expect_used,
            reason = "CG-021-no-panic-on-reachable-failure: The infallible invalidation \
                      constructor requires a valid covering range; unvalidated inputs use try_new"
        )]
        Self::try_new(object, object_start, object_len, hits)
            .expect("invalidate work range must be valid and cover every hit")
    }

    /// Returns the backing object that emitted this work.
    pub const fn object(&self) -> VmObjectId {
        self.object
    }

    /// Returns the invalidated object byte start.
    pub const fn object_start(&self) -> u64 {
        self.object_start
    }

    /// Returns the invalidated object byte length.
    pub const fn object_len(&self) -> usize {
        self.object_len
    }

    /// Returns the invalidated object exclusive end offset.
    pub const fn object_end(&self) -> u64 {
        self.object_start.saturating_add(self.object_len as u64)
    }

    /// Returns the view hits covered by this work item.
    pub fn hits(&self) -> &[ObjectViewHit] {
        &self.hits
    }

    /// Returns whether no view was hit.
    pub fn is_empty(&self) -> bool {
        self.hits.is_empty()
    }

    /// Returns whether this work item explicitly carries `hit`.
    pub fn contains_hit(&self, hit: &ObjectViewHit) -> bool {
        self.hits.iter().any(|it| it == hit)
    }

    /// Creates an address-space request for one hit carried by this work item.
    pub fn request_for_hit(&self, hit: &ObjectViewHit) -> Option<ObjectInvalidateRequest> {
        self.contains_hit(hit)
            .then(|| ObjectInvalidateRequest::new(self.object, hit.clone()))
    }

    /// Creates an address-space request for a subrange of one carried hit.
    pub fn request_for_subhit(
        &self,
        original: &ObjectViewHit,
        subhit: &ObjectViewHit,
    ) -> Option<ObjectInvalidateRequest> {
        if !self.contains_hit(original)
            || original.view() != subhit.view()
            || subhit.object_start() < original.object_start()
            || subhit.object_end() > original.object_end()
        {
            return None;
        }
        Some(ObjectInvalidateRequest::new(self.object, subhit.clone()))
    }
}

/// Neutral request sent from object-side rmap infrastructure to one address
/// space consumer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObjectInvalidateRequest {
    object: VmObjectId,
    hit: ObjectViewHit,
}

impl ObjectInvalidateRequest {
    /// Creates one address-space invalidate request derived from object work.
    pub const fn new(object: VmObjectId, hit: ObjectViewHit) -> Self {
        Self { object, hit }
    }

    /// Returns the stable backing object identity.
    pub const fn object(&self) -> VmObjectId {
        self.object
    }

    /// Returns the invalidation hit carried by this request.
    pub const fn hit(&self) -> &ObjectViewHit {
        &self.hit
    }

    /// Creates one invalidate request by intersecting an object event with a
    /// registered view.
    pub fn from_page_hit(
        object: VmObjectId,
        view: &MappingView,
        object_start: u64,
        object_len: usize,
    ) -> Option<Self> {
        view.page_hit(object_start, object_len)
            .map(|hit| Self::new(object, hit))
    }
}

/// Reverse-mapping callback owned by one registered object view.
pub trait MappingViewNotifier: Send + Sync {
    /// Applies object-driven invalidate work to the registered VMA view.
    fn invalidate(&self, work: &ObjectInvalidateWork, hit: &ObjectViewHit);
}

unittest::path_mod_test!(tests, "../ut-tests/lib.rs");
