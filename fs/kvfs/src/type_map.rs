// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Type-indexed private attachment storage.

use alloc::sync::Arc;
use core::any::{Any, TypeId};

use smallvec::SmallVec;

/// Type-indexed metadata storage.
#[derive(Default)]
pub struct TypeMap(SmallVec<[(TypeId, Arc<dyn Any + Send + Sync>); 2]>);

impl TypeMap {
    /// Insert an existing `Arc<T>` by its concrete type.
    pub fn insert_arc<T: Any + Send + Sync>(&mut self, value: Arc<T>) {
        let id = TypeId::of::<T>();
        let value: Arc<dyn Any + Send + Sync> = value;
        self.insert_any(id, value);
    }

    fn insert_any(&mut self, id: TypeId, value: Arc<dyn Any + Send + Sync>) {
        if let Some((_, slot)) = self.0.iter_mut().find(|(existing, _)| *existing == id) {
            *slot = value;
        } else {
            self.0.push((id, value));
        }
    }

    /// Get a value by its concrete type.
    pub fn get<T: Any + Send + Sync>(&self) -> Option<Arc<T>> {
        self.0
            .iter()
            .find_map(|(id, value)| {
                if id == &TypeId::of::<T>() {
                    Some(value.clone())
                } else {
                    None
                }
            })
            .and_then(|value| value.downcast().ok())
    }
}
