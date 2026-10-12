//! [`FlattenObjects`] is a container that stores numbered objects.
//!
//! Objects can be added to the [`FlattenObjects`], a unique ID will be assigned
//! to the object. The ID can be used to retrieve the object later.
//!
//! # Example
//!
//! ```
//! use flatten_objects::FlattenObjects;
//!
//! let mut objects = FlattenObjects::<u32, 20>::new();
//!
//! // Add `23` 10 times and assign them IDs from 0 to 9.
//! for i in 0..=9 {
//!     objects.add_at(i, 23).unwrap();
//!     assert!(objects.is_assigned(i));
//! }
//!
//! // Remove the object with ID 6.
//! assert_eq!(objects.remove(6), Some(23));
//! assert!(!objects.is_assigned(6));
//!
//! // Add `42` (the ID 6 is available now).
//! let id = objects.add(42).unwrap();
//! assert_eq!(id, 6);
//! assert!(objects.is_assigned(id));
//! assert_eq!(objects.get(id), Some(&42));
//! assert_eq!(objects.remove(id), Some(42));
//! assert!(!objects.is_assigned(id));
//! ```

#![no_std]

use core::mem::MaybeUninit;

use bitmaps::{Bitmap, Bits, BitsImpl, Iter};

/// A container that stores numbered objects.
///
/// See the [crate-level documentation](crate) for more details.
///
/// `CAP` is the maximum number of objects that can be held. It also equals the
/// maximum ID that can be assigned plus one. Currently, `CAP` must not be
/// greater than 1024.
pub struct FlattenObjects<T, const CAP: usize>
where
    BitsImpl<{ CAP }>: Bits,
{
    objects: [MaybeUninit<T>; CAP],
    id_bitmap: Bitmap<CAP>,
    count: usize,
}

impl<T, const CAP: usize> FlattenObjects<T, CAP>
where
    BitsImpl<{ CAP }>: Bits,
{
    /// Creates a new empty `FlattenObjects`.
    ///
    /// # Panics
    ///
    /// Panics if `CAP` is greater than 1024.
    ///
    /// # Example
    ///
    /// ```
    /// use flatten_objects::FlattenObjects;
    ///
    /// let objects = FlattenObjects::<u32, 20>::new();
    /// assert_eq!(objects.capacity(), 20);
    /// ```
    ///
    /// ```compile_fail
    /// use flatten_objects::FlattenObjects;
    ///
    /// let objects = FlattenObjects::<u32, 1025>::new();
    /// ```
    pub const fn new() -> Self {
        Self {
            objects: [const { MaybeUninit::uninit() }; CAP],
            // SAFETY: zero initialization is OK for `id_bitmap` (an array of integers).
            id_bitmap: unsafe { MaybeUninit::zeroed().assume_init() },
            count: 0,
        }
    }

    /// Returns the maximum number of objects that can be held.
    ///
    /// It also equals the maximum ID that can be assigned plus one.
    ///
    /// # Example
    ///
    /// ```
    /// use flatten_objects::FlattenObjects;
    ///
    /// let objects = FlattenObjects::<u32, 20>::new();
    /// assert_eq!(objects.capacity(), 20);
    /// ```
    #[inline]
    pub const fn capacity(&self) -> usize {
        CAP
    }

    /// Returns the number of objects that have been added.
    ///
    /// # Example
    ///
    /// ```
    /// use flatten_objects::FlattenObjects;
    ///
    /// let mut objects = FlattenObjects::<u32, 20>::new();
    /// assert_eq!(objects.count(), 0);
    /// objects.add(23); // Assign ID 0.
    /// assert_eq!(objects.count(), 1);
    /// objects.add(42); // Assign ID 1.
    /// assert_eq!(objects.count(), 2);
    /// objects.remove(0); // ID 0 is assigned.
    /// assert_eq!(objects.count(), 1);
    /// objects.remove(10); // ID 10 is not assigned.
    /// assert_eq!(objects.count(), 1);
    /// ```
    #[inline]
    pub const fn count(&self) -> usize {
        self.count
    }

    /// Checks if the given `id` is assigned.
    ///
    /// Returns `false` if the `id` is out of range.
    ///
    /// # Example
    ///
    /// ```
    /// use flatten_objects::FlattenObjects;
    ///
    /// let mut objects = FlattenObjects::<u32, 20>::new();
    /// objects.add(23); // Assign ID 0.
    /// objects.add_at(5, 42); // Assign ID 5.
    /// assert!(objects.is_assigned(0));
    /// assert!(!objects.is_assigned(1));
    /// assert!(objects.is_assigned(5));
    /// assert!(!objects.is_assigned(10));
    /// ```
    #[inline]
    pub fn is_assigned(&self, id: usize) -> bool {
        id < CAP && self.id_bitmap.get(id)
    }

    /// Returns the reference of the element with the given `id` if it already
    /// be assigned. Otherwise, returns `None`.
    ///
    /// # Example
    ///
    /// ```
    /// use flatten_objects::FlattenObjects;
    ///
    /// let mut objects = FlattenObjects::<u32, 20>::new();
    /// objects.add(23); // Assign ID 0.
    /// objects.add_at(5, 42); // Assign ID 5.
    /// assert_eq!(objects.get(0), Some(&23));
    /// assert_eq!(objects.get(1), None);
    /// assert_eq!(objects.get(5), Some(&42));
    /// assert_eq!(objects.get(10), None);
    /// ```
    #[inline]
    pub fn get(&self, id: usize) -> Option<&T> {
        if self.is_assigned(id) {
            // SAFETY: the object at `id` should be initialized by `add` or
            // `add_at`.
            unsafe { Some(self.objects[id].assume_init_ref()) }
        } else {
            None
        }
    }

    /// Returns the mutable reference of the element with the given `id` if it
    /// exists. Otherwise, returns `None`.
    ///
    /// # Example
    ///
    /// ```
    /// use flatten_objects::FlattenObjects;
    ///
    /// let mut objects = FlattenObjects::<u32, 20>::new();
    /// objects.add(23); // Assign ID 0.
    /// objects.add_at(5, 42); // Assign ID 5.
    /// *objects.get_mut(0).unwrap() = 24;
    /// assert_eq!(objects.get_mut(1), None);
    /// *objects.get_mut(5).unwrap() = 43;
    /// assert_eq!(objects.get_mut(10), None);
    /// assert_eq!(objects.get(0), Some(&24));
    /// assert_eq!(objects.get(5), Some(&43));
    /// ```
    #[inline]
    pub fn get_mut(&mut self, id: usize) -> Option<&mut T> {
        if self.is_assigned(id) {
            // SAFETY: the object at `id` should be initialized by `add` or
            // `add_at`.
            unsafe { Some(self.objects[id].assume_init_mut()) }
        } else {
            None
        }
    }

    /// Add an object and assigns it the smallest available ID.
    ///
    /// Returns the ID if there is one available. Otherwise, returns the object
    /// itself wrapped in `Err`.
    ///
    /// # Example
    ///
    /// ```
    /// use flatten_objects::FlattenObjects;
    ///
    /// let mut objects = FlattenObjects::<u32, 3>::new();
    /// assert_eq!(objects.add(23), Ok(0));
    /// assert_eq!(objects.add(42), Ok(1));
    /// assert_eq!(objects.add(23), Ok(2));
    /// assert_eq!(objects.add(42), Err(42));
    /// objects.remove(1);
    /// assert_eq!(objects.add(42), Ok(1));
    /// ```
    pub fn add(&mut self, value: T) -> Result<usize, T> {
        match self.id_bitmap.first_false_index() {
            Some(id) if id < CAP => {
                self.count += 1;
                self.id_bitmap.set(id, true);
                self.objects[id].write(value);
                Ok(id)
            }
            _ => Err(value),
        }
    }

    /// Adds an object at the lowest free ID in `[minimum, end)`.
    ///
    /// The exclusive end is clipped to capacity. Empty, reversed, out-of-capacity,
    /// or fully occupied ranges return the uninserted object without mutation.
    /// Search uses the existing occupancy bitmap's word search; it does not scan
    /// individual occupied IDs or maintain a second allocation index.
    pub fn add_in_range(&mut self, minimum: usize, end: usize, value: T) -> Result<usize, T> {
        let end = end.min(CAP);
        if minimum >= end {
            return Err(value);
        }
        // next_false_index is exclusive, so check the first allowed bit itself.
        let id = if !self.id_bitmap.get(minimum) {
            Some(minimum)
        } else {
            self.id_bitmap.next_false_index(minimum)
        };
        match id {
            Some(id) if id < end => self.add_at(id, value),
            _ => Err(value),
        }
    }

    /// Add an object with the given ID.
    ///
    /// Returns the ID if the object is added successfully. Otherwise, returns
    /// the object itself wrapped in `Err`.
    ///
    /// # Example
    ///
    /// ```
    /// use flatten_objects::FlattenObjects;
    ///
    /// let mut objects = FlattenObjects::<u32, 20>::new();
    /// assert_eq!(objects.add_at(5, 23), Ok(5));
    /// assert_eq!(objects.add_at(5, 42), Err(42));
    /// assert_eq!(objects.add_at(20, 42), Err(42));
    /// ```
    pub fn add_at(&mut self, id: usize, value: T) -> Result<usize, T> {
        if id >= CAP || self.is_assigned(id) {
            return Err(value);
        }
        self.count += 1;
        self.id_bitmap.set(id, true);
        self.objects[id].write(value);
        Ok(id)
    }

    /// Adds an object with the given ID, replacing and returning the old object
    /// if the ID is already assigned.
    ///
    /// Returns the ID if the object is added successfully. Returns `Err(Some(old))`
    /// if the ID is already assigned. Returns `Err(None)` if the ID is out of
    /// range.
    ///
    /// # Example
    ///
    /// ```
    /// use flatten_objects::FlattenObjects;
    ///
    /// let mut objects = FlattenObjects::<u32, 20>::new();
    /// assert_eq!(objects.add_or_replace_at(5, 23), Ok(5));
    /// assert_eq!(objects.add_or_replace_at(5, 42), Err(Some(23)));
    /// assert_eq!(objects.get(5), Some(&42));
    /// assert_eq!(objects.add_or_replace_at(20, 42), Err(None));
    /// ```
    pub fn add_or_replace_at(&mut self, id: usize, value: T) -> Result<usize, Option<T>> {
        if id >= CAP {
            return Err(None);
        }

        if self.is_assigned(id) {
            // SAFETY: the object at `id` should be initialized by `add` or
            // `add_at`, and can not be retrieved by `get` or `get_mut` unless
            // it be added again.
            let old = unsafe { Some(self.objects[id].assume_init_read()) };
            self.objects[id].write(value);

            Err(old)
        } else {
            self.count += 1;
            self.id_bitmap.set(id, true);
            self.objects[id].write(value);

            Ok(id)
        }
    }

    /// Removes and returns the object with the given ID.
    ///
    /// After this operation, the ID is freed and can be assigned for next
    /// object again.
    ///
    /// # Example
    ///
    /// ```
    /// use flatten_objects::FlattenObjects;
    ///
    /// let mut objects = FlattenObjects::<u32, 20>::new();
    /// let id = objects.add(23).unwrap();
    /// assert_eq!(objects.remove(id), Some(23));
    /// assert!(!objects.is_assigned(id));
    /// assert_eq!(objects.remove(id), None);
    /// ```
    pub fn remove(&mut self, id: usize) -> Option<T> {
        if self.is_assigned(id) {
            self.id_bitmap.set(id, false);
            self.count -= 1;
            // SAFETY: the object at `id` should be initialized by `add` or
            // `add_at`, and can not be retrieved by `get` or `get_mut` unless
            // it be added again.
            unsafe { Some(self.objects[id].assume_init_read()) }
        } else {
            None
        }
    }

    /// Gets an iterator over the IDs of the assigned objects.
    ///
    /// # Example
    ///
    /// ```
    /// use flatten_objects::FlattenObjects;
    ///
    /// let mut objects = FlattenObjects::<u32, 20>::new();
    /// objects.add(23); // Assign ID 0.
    /// objects.add_at(5, 42); // Assign ID 5.
    /// let mut iter = objects.ids();
    /// assert_eq!(iter.next(), Some(0));
    /// assert_eq!(iter.next(), Some(5));
    /// assert_eq!(iter.next(), None);
    /// ```
    #[inline]
    pub fn ids(&self) -> Iter<'_, CAP> {
        self.id_bitmap.into_iter()
    }
}

impl<T, const CAP: usize> Default for FlattenObjects<T, CAP>
where
    BitsImpl<{ CAP }>: Bits,
{
    fn default() -> Self {
        Self::new()
    }
}

impl<T: Clone, const CAP: usize> Clone for FlattenObjects<T, CAP>
where
    BitsImpl<{ CAP }>: Bits,
{
    fn clone(&self) -> Self {
        let mut cloned = Self::new();
        cloned.count = self.count;
        cloned.id_bitmap = self.id_bitmap;
        for id in &self.id_bitmap {
            cloned.objects[id].write(unsafe { self.objects[id].assume_init_ref() }.clone());
        }
        cloned
    }
}

impl<T, const CAP: usize> Drop for FlattenObjects<T, CAP>
where
    BitsImpl<{ CAP }>: Bits,
{
    fn drop(&mut self) {
        for id in &self.id_bitmap {
            unsafe { self.objects[id].assume_init_drop() };
        }
    }
}
