// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! EEVDF augmentation over intrusive-collections' owning red-black tree.
//!
//! The library owns nodes and performs every rotation/recolor. Custom RBTreeOps
//! record nodes whose children changed; after the complete edit, their final
//! parent paths are repaired. No propagation follows transient rotation links.
//! The private context has one allocation per queue; enqueue/dequeue allocate
//! nothing. Access is serialized by the caller's RQ lock or exclusive teardown.

use alloc::sync::Arc;
use core::{
    cell::Cell,
    ptr::NonNull,
    sync::atomic::{AtomicU8, Ordering},
};

use intrusive_collections::{
    Adapter, DefaultLinkOps, KeyAdapter, LinkOps, PointerOps, RBTree,
    rbtree::{Color, RBTreeOps},
};

pub(crate) type DeadlineKey = (i64, u64);
type NodePtr = NonNull<FairLink>;
type Owner<A> = <<A as Adapter>::PointerOps as PointerOps>::Pointer;
type Value<A> = <<A as Adapter>::PointerOps as PointerOps>::Value;

const UNLINKED: u8 = 0;
const RED: u8 = 1;
const BLACK: u8 = 2;
// Keep a removed node reserved until its dirty-list entry has been consumed.
const RETIRED: u8 = 3;

#[derive(Clone, Copy)]
struct Node {
    left: Option<NodePtr>,
    right: Option<NodePtr>,
    parent: Option<NodePtr>,
    min_vruntime: i64,
    dirty_next: Option<NodePtr>,
    is_dirty: bool,
}

impl Node {
    const EMPTY: Self = Self {
        left: None,
        right: None,
        parent: None,
        min_vruntime: 0,
        dirty_next: None,
        is_dirty: false,
    };
}

/// Embedded metadata; only atomic membership may be observed without the RQ.
pub(crate) struct FairLink {
    node: Cell<Node>,
    state: AtomicU8,
}

// SAFETY: only the owning tree accesses Cell metadata under RQ/exclusive-edit
// synchronization. Atomic membership is the sole lock-free accessor. A retired
// link remains reserved until augmentation repair ends, preventing early reuse.
unsafe impl Sync for FairLink {}
// SAFETY: adapter owners keep linked storage stationary. Moving an unlinked
// value is allowed; transferring a queue transfers access without moving nodes.
unsafe impl Send for FairLink {}

impl FairLink {
    pub(crate) const fn new() -> Self {
        Self {
            node: Cell::new(Node::EMPTY),
            state: AtomicU8::new(UNLINKED),
        }
    }

    pub(crate) fn is_linked(&self) -> bool {
        // An observation only; this does not publish topology/scheduling data.
        self.state.load(Ordering::Relaxed) != UNLINKED
    }
}

fn with_link<R>(ptr: NodePtr, f: impl FnOnce(&FairLink) -> R) -> R {
    // SAFETY: private callers use live links held by the owning RBTree, a
    // pending insertion or the owner retained across removal/repair. Adapter
    // projections preserve those addresses; references cannot escape f.
    f(unsafe { ptr.as_ref() })
}

fn node(ptr: NodePtr) -> Node {
    with_link(ptr, |link| link.node.get())
}

fn store(ptr: NodePtr, data: Node) {
    with_link(ptr, |link| link.node.set(data));
}

fn state(ptr: NodePtr) -> u8 {
    with_link(ptr, |link| link.state.load(Ordering::Relaxed))
}

#[derive(Clone, Copy, Default)]
pub(crate) struct FairLinkOps;

impl DefaultLinkOps for FairLink {
    type Ops = FairLinkOps;

    const NEW: Self::Ops = FairLinkOps;
}

// SAFETY: Acquire/Release transfers exclusive node use. The caller keeps the
// adapter owner live until release completes. Only the bound tree ops may edit
// topology; this base type deliberately does not implement RBTreeOps.
unsafe impl LinkOps for FairLinkOps {
    type LinkPtr = NodePtr;

    /// # Safety
    /// ptr must be a live, aligned link projected from the pending owner.
    unsafe fn acquire_link(&mut self, ptr: NodePtr) -> bool {
        with_link(ptr, |link| {
            link.state
                .compare_exchange(UNLINKED, RED, Ordering::Acquire, Ordering::Relaxed)
                .is_ok()
        })
    }

    /// # Safety
    /// ptr must be fully detached, no longer pending repair, and owner-retained.
    unsafe fn release_link(&mut self, ptr: NodePtr) {
        store(ptr, Node::EMPTY);
        with_link(ptr, |link| link.state.store(UNLINKED, Ordering::Release));
    }
}

/// Stable projections and scheduling fields provided under the caller's RQ lock.
///
/// # Safety
/// `vruntime` must not panic or reenter the tree: repair may still retain
/// pending references to a removed node whose owner is held by the caller.
/// Linked keys/vruntime must stay unchanged until removal.
pub(crate) unsafe trait FairTreeAdapter:
    Sized + Adapter<LinkOps = FairLinkOps> + for<'a> KeyAdapter<'a, Key = DeadlineKey>
{
    fn vruntime(&self, value: &Value<Self>) -> i64;
}

struct MutationState {
    root: Cell<Option<NodePtr>>,
    dirty: Cell<Option<NodePtr>>,
    retired: Cell<Option<NodePtr>>,
    is_clearing: Cell<bool>,
}

// SAFETY: this private state is shared only by a FairTree and its bound adapter.
// RBTree edits require &mut tree, queries use immutable completed topology, and
// Drop has exclusive access. No context handle escapes; linked/retired owners
// keep every pointer used during repair live. The caller supplies RQ locking.
unsafe impl Sync for MutationState {}
// SAFETY: moving the owning FairTree transfers both context handles together;
// node allocations remain stationary and are retained by the library tree.
unsafe impl Send for MutationState {}

struct Context<A: FairTreeAdapter> {
    adapter: A,
    mutation: MutationState,
}

impl<A: FairTreeAdapter> Context<A> {
    fn value(&self, ptr: NodePtr) -> &Value<A> {
        // SAFETY: ptr is a library-owned live link (or the pending insertion).
        // Reverse projection preserves the original PointerOps provenance.
        let raw = unsafe { self.adapter.get_value(ptr) };
        // SAFETY: the retained owner keeps this initialized value live through
        // the context borrow; no exclusive reference is created.
        unsafe { &*raw }
    }

    fn mark_dirty(&self, ptr: NodePtr) {
        let mut data = node(ptr);
        if !data.is_dirty {
            data.is_dirty = true;
            data.dirty_next = self.mutation.dirty.replace(Some(ptr));
            store(ptr, data);
        }
    }

    fn repair(&self) {
        while let Some(ptr) = self.mutation.dirty.get() {
            let mut data = node(ptr);
            self.mutation.dirty.set(data.dirty_next);
            data.is_dirty = false;
            data.dirty_next = None;
            store(ptr, data);
            if state(ptr) == RETIRED {
                continue;
            }
            let mut path = Some(ptr);
            while let Some(current) = path {
                let mut data = node(current);
                data.min_vruntime = self.adapter.vruntime(self.value(current));
                for child in [data.left, data.right].into_iter().flatten() {
                    data.min_vruntime = data.min_vruntime.min(node(child).min_vruntime);
                }
                store(current, data);
                path = data.parent;
            }
        }
        if let Some(ptr) = self.mutation.retired.take() {
            // SAFETY: the removed owner is held by FairTree::remove. All dirty
            // references have now been consumed, so another tree may reuse it.
            unsafe { FairLinkOps.release_link(ptr) };
        }
    }
}

struct AugmentOps<A: FairTreeAdapter> {
    context: Arc<Context<A>>,
}

// SAFETY: the library delegates its link reservation here. Normal unlink keeps
// the node RETIRED until post-edit repair, with the returned owner retained.
// Clear/drop has no repair phase and releases each node after reading children.
unsafe impl<A: FairTreeAdapter> LinkOps for AugmentOps<A> {
    type LinkPtr = NodePtr;

    /// # Safety
    /// ptr is a live adapter-projected link for the library's pending owner.
    unsafe fn acquire_link(&mut self, ptr: NodePtr) -> bool {
        // SAFETY: the library supplies the pending live owner's link.
        if !unsafe { FairLinkOps.acquire_link(ptr) } {
            return false;
        }
        let min_vruntime = self.context.adapter.vruntime(self.context.value(ptr));
        store(
            ptr,
            Node {
                min_vruntime,
                ..Node::EMPTY
            },
        );
        true
    }

    /// # Safety
    /// The library has removed ptr from its topology and still retains its owner.
    unsafe fn release_link(&mut self, ptr: NodePtr) {
        let mutation = &self.context.mutation;
        if mutation.root.get() == Some(ptr) {
            mutation.root.set(None);
        }
        if mutation.is_clearing.get() {
            // SAFETY: clear already saved both children and will never read
            // this node again; no dirty-list traversal follows teardown.
            unsafe { FairLinkOps.release_link(ptr) };
        } else {
            with_link(ptr, |link| link.state.store(RETIRED, Ordering::Relaxed));
            mutation.retired.set(Some(ptr));
        }
    }
}

// SAFETY: these operations preserve exactly the child/parent/color requested by
// the library. Setters do not allocate, panic, follow transient ancestor links
// or invoke user callbacks. Dirty recording is O(1) and summaries are repaired
// only after the library completes balancing. Color is separate from pointers.
unsafe impl<A: FairTreeAdapter> RBTreeOps for AugmentOps<A> {
    /// # Safety
    /// ptr is a live link retained by the library operation.
    unsafe fn left(&self, ptr: NodePtr) -> Option<NodePtr> {
        node(ptr).left
    }

    /// # Safety
    /// ptr is a live link retained by the library operation.
    unsafe fn right(&self, ptr: NodePtr) -> Option<NodePtr> {
        node(ptr).right
    }

    /// # Safety
    /// ptr is a live link retained by the library operation.
    unsafe fn parent(&self, ptr: NodePtr) -> Option<NodePtr> {
        node(ptr).parent
    }

    /// # Safety
    /// ptr is a live, acquired link visited before release.
    unsafe fn color(&self, ptr: NodePtr) -> Color {
        if state(ptr) == RED {
            Color::Red
        } else {
            Color::Black
        }
    }

    /// # Safety
    /// ptr/left are live links belonging to the current library mutation.
    unsafe fn set_left(&mut self, ptr: NodePtr, left: Option<NodePtr>) {
        let mut data = node(ptr);
        data.left = left;
        store(ptr, data);
        self.context.mark_dirty(ptr);
    }

    /// # Safety
    /// ptr/right are live links belonging to the current library mutation.
    unsafe fn set_right(&mut self, ptr: NodePtr, right: Option<NodePtr>) {
        let mut data = node(ptr);
        data.right = right;
        store(ptr, data);
        self.context.mark_dirty(ptr);
    }

    /// # Safety
    /// ptr/parent are live links belonging to the current library mutation.
    unsafe fn set_parent(&mut self, ptr: NodePtr, parent: Option<NodePtr>) {
        let mut data = node(ptr);
        data.parent = parent;
        store(ptr, data);
        if parent.is_none() {
            self.context.mutation.root.set(Some(ptr));
        }
    }

    /// # Safety
    /// ptr is an acquired node still belonging to the library mutation.
    unsafe fn set_color(&mut self, ptr: NodePtr, color: Color) {
        let color = match color {
            Color::Red => RED,
            Color::Black => BLACK,
        };
        with_link(ptr, |link| link.state.store(color, Ordering::Relaxed));
    }
}

struct BoundAdapter<A: FairTreeAdapter> {
    ops: AugmentOps<A>,
}

// SAFETY: projections and pointer ownership delegate to the exact original
// adapter retained in context. Only LinkOps changes, to add augmentation.
unsafe impl<A: FairTreeAdapter> Adapter for BoundAdapter<A> {
    type LinkOps = AugmentOps<A>;
    type PointerOps = A::PointerOps;

    /// # Safety
    /// ptr must have been projected by this adapter from a still-live owner.
    unsafe fn get_value(&self, ptr: NodePtr) -> *const Value<A> {
        // SAFETY: the original adapter produced this same link projection.
        unsafe { self.ops.context.adapter.get_value(ptr) }
    }

    /// # Safety
    /// value must be a live initialized value accepted by the original adapter.
    unsafe fn get_link(&self, value: *const Value<A>) -> NodePtr {
        // SAFETY: projection delegates the library's original owner pointer.
        unsafe { self.ops.context.adapter.get_link(value) }
    }

    fn link_ops(&self) -> &Self::LinkOps {
        &self.ops
    }

    fn link_ops_mut(&mut self) -> &mut Self::LinkOps {
        &mut self.ops
    }

    fn pointer_ops(&self) -> &Self::PointerOps {
        self.ops.context.adapter.pointer_ops()
    }
}

impl<'a, A: FairTreeAdapter> KeyAdapter<'a> for BoundAdapter<A> {
    type Key = DeadlineKey;

    fn get_key(&self, value: &'a Value<A>) -> DeadlineKey {
        self.ops.context.adapter.get_key(value)
    }
}

pub(crate) struct FairTree<A: FairTreeAdapter> {
    tree: RBTree<BoundAdapter<A>>,
    context: Arc<Context<A>>,
}

// SAFETY: both private context handles and all retained owners move together.
// No handle escapes the wrapper; adapter values need only Send for this move.
unsafe impl<A: FairTreeAdapter + Send> Send for FairTree<A> where Owner<A>: Send {}
// SAFETY: shared methods only inspect completed topology and Sync values/owners.
// Library edits and context repair require exclusive wrapper access/RQ locking.
unsafe impl<A: FairTreeAdapter + Sync> Sync for FairTree<A>
where
    Value<A>: Sync,
    Owner<A>: Sync,
{
}

impl<A: FairTreeAdapter> FairTree<A> {
    pub(crate) fn new(adapter: A) -> Self {
        let context = Arc::new(Context {
            adapter,
            mutation: MutationState {
                root: Cell::new(None),
                dirty: Cell::new(None),
                retired: Cell::new(None),
                is_clearing: Cell::new(false),
            },
        });
        let tree = RBTree::new(BoundAdapter {
            ops: AugmentOps {
                context: context.clone(),
            },
        });
        Self { tree, context }
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.tree.is_empty()
    }

    pub(crate) fn insert(&mut self, owner: Owner<A>) {
        self.tree.insert(owner);
        self.context.repair();
    }

    pub(crate) fn remove(&mut self, key: DeadlineKey) -> Option<Owner<A>> {
        let owner = self.tree.find_mut(&key).remove()?;
        // Keep the returned owner live while consuming dirty references to the
        // retired node. Release membership only after repair, before returning.
        self.context.repair();
        Some(owner)
    }

    pub(crate) fn find(&self, key: DeadlineKey) -> Option<&Value<A>> {
        self.tree.find(&key).get()
    }

    #[cfg(any(test, unittest))]
    pub(crate) fn first(&self) -> Option<&Value<A>> {
        self.tree.front().get()
    }

    pub(crate) fn clone_first(&self) -> Option<Owner<A>>
    where
        Owner<A>: Clone,
    {
        self.tree.front().clone_pointer()
    }

    pub(crate) fn min_vruntime(&self) -> Option<i64> {
        self.context
            .mutation
            .root
            .get()
            .map(|ptr| node(ptr).min_vruntime)
    }

    #[cfg(any(test, unittest))]
    fn search(&self, avg: i64, mut visit: impl FnMut()) -> Option<DeadlineKey> {
        let mut next = self.context.mutation.root.get();
        while let Some(ptr) = next {
            visit();
            let data = node(ptr);
            if data.min_vruntime > avg {
                return None;
            }
            if data.left.is_some_and(|left| node(left).min_vruntime <= avg) {
                next = data.left;
            } else if self.context.adapter.vruntime(self.context.value(ptr)) <= avg {
                return Some(self.context.adapter.get_key(self.context.value(ptr)));
            } else {
                next = data.right;
            }
        }
        None
    }

    pub(crate) fn iter(&self) -> impl Iterator<Item = &Value<A>> {
        self.tree.iter()
    }
}

impl<A: FairTreeAdapter> Drop for FairTree<A> {
    fn drop(&mut self) {
        self.context.mutation.is_clearing.set(true);
        self.tree.clear();
        // Teardown may free previously dirty nodes; discard pointers without
        // traversing them. The library tree is empty before context release.
        self.context.mutation.dirty.set(None);
        self.context.mutation.retired.set(None);
        self.context.mutation.root.set(None);
    }
}

#[cfg(any(test, unittest))]
impl<A: FairTreeAdapter> FairTree<A> {
    pub(crate) fn search_for_test(&self, avg: i64) -> (Option<DeadlineKey>, usize) {
        let mut visits = 0;
        let key = self.search(avg, || visits += 1);
        (key, visits)
    }

    pub(crate) fn height_for_test(&self) -> usize {
        self.validate(self.context.mutation.root.get(), None, None, None)
            .map_or(0, |v| v.1)
    }

    pub(crate) fn valid_for_test(&self) -> bool {
        let mutation = &self.context.mutation;
        mutation.dirty.get().is_none()
            && mutation.retired.get().is_none()
            && mutation.root.get().is_none_or(|ptr| state(ptr) == BLACK)
            && self
                .validate(mutation.root.get(), None, None, None)
                .is_some()
    }

    fn validate(
        &self,
        ptr: Option<NodePtr>,
        parent: Option<NodePtr>,
        lower: Option<DeadlineKey>,
        upper: Option<DeadlineKey>,
    ) -> Option<(usize, usize, i64, usize)> {
        let Some(ptr) = ptr else {
            return Some((0, 0, i64::MAX, 1));
        };
        let data = node(ptr);
        let key = self.context.adapter.get_key(self.context.value(ptr));
        let is_red = state(ptr) == RED;
        if data.parent != parent
            || !matches!(state(ptr), RED | BLACK)
            || data.is_dirty
            || lower.is_some_and(|v| key < v)
            || upper.is_some_and(|v| key > v)
            || (is_red
                && [data.left, data.right]
                    .into_iter()
                    .flatten()
                    .any(|ptr| state(ptr) == RED))
        {
            return None;
        }
        let left = self.validate(data.left, Some(ptr), lower, Some(key))?;
        let right = self.validate(data.right, Some(ptr), Some(key), upper)?;
        let min = self
            .context
            .adapter
            .vruntime(self.context.value(ptr))
            .min(left.2)
            .min(right.2);
        (left.3 == right.3 && data.min_vruntime == min).then_some((
            1 + left.0 + right.0,
            1 + left.1.max(right.1),
            min,
            left.3 + usize::from(!is_red),
        ))
    }
}
