// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Serialized parent/child relation updates.
//!
//! A topology write gathers every relation it may touch, takes those devices'
//! relation spinlocks in ascending [`DeviceId`] order (independent of parent /
//! child role), revalidates the snapshot under the locks, and retries a bounded
//! number of times if the topology changed in between. This is what rules out
//! role-order ABBA deadlocks and unbounded locking; see the design's §7.7.
//!
//! All functions here use only registry and relation spinlocks. They never
//! sleep and must not be called while already holding a relation lock.

use alloc::{sync::Arc, vec::Vec};

use driver_base::DriverError;

use crate::{DeviceId, DeviceObject, registry::device_registry};

/// Maximum number of parent edges one ancestor walk may traverse.
const MAX_PARENT_DEPTH: usize = 64;
/// Maximum number of snapshot/validate retries for one topology write.
const MAX_TOPOLOGY_RETRIES: usize = 8;

/// One ancestor-chain edge captured with a single relation-lock read.
struct AncestorEdge {
    node: Arc<DeviceObject>,
    parent: Option<DeviceId>,
}

/// A validated input snapshot for an attach operation.
struct AttachSnapshot {
    old_parent: Option<Arc<DeviceObject>>,
    child_old_parent: Option<DeviceId>,
    ancestors: Vec<AncestorEdge>,
    /// Child, new parent, old parent and all ancestor nodes, deduplicated and
    /// sorted by ascending id.
    nodes: Vec<Arc<DeviceObject>>,
}

/// Take a single-locked snapshot of the parent chain starting at `start`.
///
/// A missing ancestor object means the topology is being torn down or the
/// topology is broken; report `ResourceBusy` and let the caller retry rather
/// than writing over a half-removed chain. A repeated id means an existing
/// cycle: `InvalidInput`. The walk is bounded by `MAX_PARENT_DEPTH`.
fn ancestor_edges(start: DeviceId) -> Result<Vec<AncestorEdge>, DriverError> {
    let mut edges = Vec::with_capacity(MAX_PARENT_DEPTH);
    let mut visited = Vec::with_capacity(MAX_PARENT_DEPTH + 1);
    let mut current = start;

    loop {
        if visited.contains(&current) {
            return Err(DriverError::InvalidInput);
        }
        visited.push(current);

        let node = {
            let index = device_registry();
            index
                .find_device(current)
                .ok_or(DriverError::ResourceBusy)?
        };
        let parent = node.parent();
        edges.push(AncestorEdge { node, parent });

        let Some(parent) = parent else {
            return Ok(edges);
        };
        if edges.len() >= MAX_PARENT_DEPTH {
            return Err(DriverError::ResourceBusy);
        }
        current = parent;
    }
}

fn relation_index(nodes: &[Arc<DeviceObject>], id: DeviceId) -> Option<usize> {
    nodes.binary_search_by_key(&id, |node| node.id()).ok()
}

/// Build the sorted, deduplicated lock set for an attach snapshot.
fn attach_nodes(
    child: &Arc<DeviceObject>,
    new_parent: &Arc<DeviceObject>,
    old_parent: Option<&Arc<DeviceObject>>,
    ancestors: &[AncestorEdge],
) -> Vec<Arc<DeviceObject>> {
    let mut nodes = Vec::with_capacity(ancestors.len() + 3);
    nodes.push(Arc::clone(child));
    nodes.push(Arc::clone(new_parent));
    if let Some(old_parent) = old_parent {
        nodes.push(Arc::clone(old_parent));
    }
    for edge in ancestors {
        nodes.push(Arc::clone(&edge.node));
    }
    nodes.sort_unstable_by_key(|node| node.id());
    nodes.dedup_by_key(|node| node.id());
    nodes
}

/// Read the input snapshot for attaching `child_id` under `parent_id`.
fn attach_snapshot(child_id: DeviceId, parent_id: DeviceId) -> Result<AttachSnapshot, DriverError> {
    let (child, new_parent, child_old_parent, old_parent) = {
        let index = device_registry();
        let child = index
            .find_device(child_id)
            .ok_or(DriverError::InvalidInput)?;
        let new_parent = index
            .find_device(parent_id)
            .ok_or(DriverError::InvalidInput)?;
        let child_old_parent = child.parent();
        let old_parent = child_old_parent.and_then(|id| index.find_device(id));
        (child, new_parent, child_old_parent, old_parent)
    };

    // `parent_id == child_id` is handled by the caller before this point; the
    // ancestor walk below would also find the child and return InvalidInput.
    let ancestors = ancestor_edges(parent_id)?;

    let nodes = attach_nodes(&child, &new_parent, old_parent.as_ref(), &ancestors);
    Ok(AttachSnapshot {
        old_parent,
        child_old_parent,
        ancestors,
        nodes,
    })
}

/// Attach (or atomically re-parent) `child_id` under `parent_id`.
///
/// The caller must already hold the child descriptor's binding lock (or be the
/// registration/adoption write context that holds it). Self-parent, an
/// existing cycle, or a cycle reachable through the new parent is rejected
/// with `InvalidInput` before any relation is written. A continuously changing
/// topology yields `ResourceBusy` after a bounded number of attempts; a
/// successful return means the relation was linked, but callers that need
/// publication-grade confirmation must also use [`recheck_membership`].
pub(crate) fn attach_device_parent_locked(
    parent_id: DeviceId,
    child_id: DeviceId,
) -> Result<(), DriverError> {
    if parent_id == child_id {
        return Err(DriverError::InvalidInput);
    }

    for _ in 0..MAX_TOPOLOGY_RETRIES {
        let snapshot = attach_snapshot(child_id, parent_id)?;
        if snapshot
            .ancestors
            .iter()
            .any(|edge| edge.node.id() == child_id)
        {
            return Err(DriverError::InvalidInput);
        }

        let AttachSnapshot {
            old_parent,
            child_old_parent,
            ancestors,
            nodes,
            ..
        } = snapshot;

        #[cfg(unittest)]
        crate::lifecycle::test_hooks::pause(
            crate::lifecycle::test_hooks::BindingPause::TopologyBeforeLink {
                parent_id,
                child_id,
            },
        );

        let applied = DeviceObject::with_relations_locks(&nodes, |relations| {
            let child_index = relation_index(&nodes, child_id)?;
            let parent_index = relation_index(&nodes, parent_id)?;

            if relations[child_index].parent != child_old_parent {
                return None;
            }
            for edge in &ancestors {
                let index = relation_index(&nodes, edge.node.id())?;
                if relations[index].parent != edge.parent {
                    return None;
                }
            }

            if child_old_parent != Some(parent_id) {
                if let Some(old_parent) = &old_parent
                    && let Some(old_index) = relation_index(&nodes, old_parent.id())
                    && let Some(pos) = relations[old_index]
                        .children
                        .iter()
                        .position(|id| *id == child_id)
                {
                    relations[old_index].children.swap_remove(pos);
                }
                relations[child_index].parent = Some(parent_id);
            }
            if !relations[parent_index].children.contains(&child_id) {
                relations[parent_index].children.push(child_id);
            }
            Some(())
        });

        if applied.is_some() {
            #[cfg(unittest)]
            crate::lifecycle::test_hooks::pause(
                crate::lifecycle::test_hooks::BindingPause::TopologyAfterLink {
                    parent_id,
                    child_id,
                },
            );
            if recheck_membership(parent_id, child_id) {
                return Ok(());
            }
            // One endpoint was unregistered between the link and the
            // membership recheck. Restore the previous edge when its parent
            // is still registered, rather than leaving a live child orphaned.
            restore_previous_parent(parent_id, child_id, child_old_parent, &nodes);
            return Err(DriverError::BadState);
        }
        // The topology changed after the snapshot; retry with a fresh one.
    }

    Err(DriverError::ResourceBusy)
}

/// Detach `child_id` from its current parent.
///
/// The caller must hold the child descriptor's binding lock. The clear is
/// conditional under the relation lock: if the child was re-parented while the
/// locks were being acquired, the write retries against the new edge.
pub(crate) fn detach_device_parent_locked(child_id: DeviceId) -> Result<(), DriverError> {
    for _ in 0..MAX_TOPOLOGY_RETRIES {
        let (child, old_parent_id, old_parent) = {
            let index = device_registry();
            let child = index
                .find_device(child_id)
                .ok_or(DriverError::InvalidInput)?;
            let old_parent_id = child.parent();
            let old_parent = old_parent_id.and_then(|id| index.find_device(id));
            (child, old_parent_id, old_parent)
        };

        let mut nodes = Vec::with_capacity(2);
        nodes.push(Arc::clone(&child));
        if let Some(old_parent) = &old_parent {
            nodes.push(Arc::clone(old_parent));
        }
        nodes.sort_unstable_by_key(|node| node.id());
        nodes.dedup_by_key(|node| node.id());

        let applied = DeviceObject::with_relations_locks(&nodes, |relations| {
            let child_index = relation_index(&nodes, child_id)?;
            if relations[child_index].parent != old_parent_id {
                return None;
            }
            if let Some(old_parent_id) = old_parent_id
                && let Some(parent_index) = relation_index(&nodes, old_parent_id)
                && let Some(pos) = relations[parent_index]
                    .children
                    .iter()
                    .position(|id| *id == child_id)
            {
                relations[parent_index].children.swap_remove(pos);
            }
            relations[child_index].parent = None;
            Some(())
        });

        if applied.is_some() {
            return Ok(());
        }
    }

    Err(DriverError::ResourceBusy)
}

/// Check that both topology endpoints are still registered.
///
/// The caller links first and calls this afterwards, so a concurrent
/// unregister either sees the link during its own membership check or is seen
/// here (see design §7.1). The registry lock is released before callers undo a
/// failed link.
pub(crate) fn recheck_membership(parent_id: DeviceId, child_id: DeviceId) -> bool {
    let index = device_registry();
    index.find_device(parent_id).is_some() && index.find_device(child_id).is_some()
}

/// Undo a failed re-parent, restoring the child's previous parent when it is
/// still registered.
///
/// The link step already removed the old parent's child entry. On failure the
/// new edge is removed and the old edge is re-added under the same ordered
/// relation locks, but only while the child still points at the new parent.
/// If the old parent is gone, fall back to the ordinary conditional undo.
fn restore_previous_parent(
    parent_id: DeviceId,
    child_id: DeviceId,
    previous_parent_id: Option<DeviceId>,
    snapshot_nodes: &[Arc<DeviceObject>],
) {
    let Some(previous_parent_id) = previous_parent_id else {
        undo_attach(parent_id, child_id);
        return;
    };
    if previous_parent_id == parent_id {
        undo_attach(parent_id, child_id);
        return;
    }

    let (child, previous_parent) = {
        let index = device_registry();
        (
            index.find_device(child_id),
            index.find_device(previous_parent_id),
        )
    };
    let (Some(child), Some(previous_parent)) = (child, previous_parent) else {
        undo_attach(parent_id, child_id);
        return;
    };

    let mut nodes = snapshot_nodes.to_vec();
    for node in core::iter::once(child).chain(core::iter::once(previous_parent)) {
        if !nodes.iter().any(|existing| existing.id() == node.id()) {
            nodes.push(node);
        }
    }
    nodes.sort_unstable_by_key(|node| node.id());
    nodes.dedup_by_key(|node| node.id());

    DeviceObject::with_relations_locks(&nodes, |relations| {
        let Some(child_index) = relation_index(&nodes, child_id) else {
            return;
        };
        let Some(new_parent_index) = relation_index(&nodes, parent_id) else {
            return;
        };
        let Some(previous_parent_index) = relation_index(&nodes, previous_parent_id) else {
            return;
        };
        let current_parent = relations[child_index].parent;
        if current_parent.is_some() && current_parent != Some(parent_id) {
            return;
        }
        if let Some(pos) = relations[new_parent_index]
            .children
            .iter()
            .position(|id| *id == child_id)
        {
            relations[new_parent_index].children.swap_remove(pos);
        }
        relations[child_index].parent = Some(previous_parent_id);
        if !relations[previous_parent_index]
            .children
            .contains(&child_id)
        {
            relations[previous_parent_index].children.push(child_id);
        }
    });

    // The previous parent may have been unregistered after the lookup. Do not
    // leave the child pointing at a parent id that is no longer in the
    // registry; the conditional undo is safe if another writer already moved
    // the child again.
    if !recheck_membership(previous_parent_id, child_id) {
        undo_attach(previous_parent_id, child_id);
    }
}

/// Undo a link that was made during the publish window when one endpoint has
/// since been unregistered.
///
/// Only the still-registered endpoint is touched, and each write is
/// conditional: a newer parent relation on the child is never cleared, and a
/// missing parent entry is already consistent.
pub(crate) fn undo_attach(parent_id: DeviceId, child_id: DeviceId) {
    let (child, parent) = {
        let index = device_registry();
        (index.find_device(child_id), index.find_device(parent_id))
    };

    if let Some(parent) = parent {
        // Idempotent set removal; no need to hold the child lock.
        parent.detach_child(child_id);
    }
    if let Some(child) = child {
        child.clear_parent_if(parent_id);
    }
}

/// Clear the topology relations of a device that is being unregistered.
///
/// `old_parent` and `children` are the snapshots taken while the registry
/// still owned the entry. The object itself is already unregistered, so each
/// relation is cleared only from the still-live peer's point of view and is
/// conditional on the expected id. Failures are logged, not propagated: the
/// registration is already ended and must not be resurrected.
pub(crate) fn clear_registration_topology(
    device: &Arc<DeviceObject>,
    old_parent: Option<DeviceId>,
    children: Vec<DeviceId>,
) {
    let device_id = device.id();
    let parent = old_parent.and_then(|id| device_registry().find_device(id));

    if let Some(parent) = parent {
        let mut nodes = [Arc::clone(&parent), Arc::clone(device)];
        nodes.sort_unstable_by_key(|node| node.id());
        DeviceObject::with_relations_locks(&nodes, |relations| {
            let Some(device_index) = relation_index(&nodes, device_id) else {
                return;
            };
            let Some(parent_index) = relation_index(&nodes, parent.id()) else {
                return;
            };
            if relations[device_index].parent == Some(parent.id()) {
                relations[device_index].parent = None;
                if let Some(pos) = relations[parent_index]
                    .children
                    .iter()
                    .position(|id| *id == device_id)
                {
                    relations[parent_index].children.swap_remove(pos);
                }
            } else if let Some(pos) = relations[parent_index]
                .children
                .iter()
                .position(|id| *id == device_id)
            {
                // Stale child entry without a matching parent edge; still remove
                // it from the live parent's set.
                relations[parent_index].children.swap_remove(pos);
            }
        });
    } else if let Some(old_parent) = old_parent {
        device.clear_parent_if(old_parent);
    }

    for child_id in children {
        let Some(child) = device_registry().find_device(child_id) else {
            continue;
        };
        let mut nodes = Vec::with_capacity(2);
        nodes.push(Arc::clone(device));
        nodes.push(child);
        nodes.sort_unstable_by_key(|node| node.id());
        nodes.dedup_by_key(|node| node.id());
        DeviceObject::with_relations_locks(&nodes, |relations| {
            let Ok(child_index) = nodes.binary_search_by_key(&child_id, |node| node.id()) else {
                return;
            };
            let Ok(parent_index) = nodes.binary_search_by_key(&device_id, |node| node.id()) else {
                return;
            };
            if relations[child_index].parent == Some(device_id) {
                relations[child_index].parent = None;
            }
            // Idempotent per (parent, child): even when the child was moved to
            // a new parent after this unregister took its snapshot, the stale
            // entry in *this* parent's set is removed. The child's new parent
            // edge is untouched.
            if let Some(pos) = relations[parent_index]
                .children
                .iter()
                .position(|id| *id == child_id)
            {
                relations[parent_index].children.swap_remove(pos);
            }
        });
    }
}
