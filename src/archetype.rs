// Copyright 2019 Google LLC
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     https://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use std::alloc::{alloc, Layout};
use std::any::TypeId;
use std::cell::UnsafeCell;
use std::mem::MaybeUninit;
use std::ptr::{self, NonNull};

use fxhash::FxHashMap;

use crate::Component;

/// A collection of entities having the same component types
pub struct Archetype {
    types: Vec<TypeInfo>,
    offsets: FxHashMap<TypeId, usize>,
    len: u32,
    entities: Box<[u32]>,
    // UnsafeCell allows unique references into `data` to be constructed while shared references
    // containing the `Archetype` exist
    data: UnsafeCell<Box<[MaybeUninit<u8>]>>,
}

impl Archetype {
    pub(crate) fn new(types: Vec<TypeInfo>) -> Self {
        debug_assert!(
            types.windows(2).all(|x| x[0] < x[1]),
            "type info not sorted"
        );
        Self {
            types,
            offsets: FxHashMap::default(),
            entities: Box::new([]),
            len: 0,
            data: UnsafeCell::new(Box::new([])),
        }
    }

    pub(crate) fn clear(&mut self) {
        for ty in &self.types {
            for index in 0..self.len {
                unsafe {
                    let removed = self
                        .get_dynamic(ty.id, ty.layout.size(), index)
                        .unwrap()
                        .as_ptr();
                    (ty.drop)(removed);
                }
            }
        }
        self.len = 0;
    }

    pub(crate) fn data<T: Component>(&self) -> Option<NonNull<T>> {
        let offset = *self.offsets.get(&TypeId::of::<T>())?;
        Some(unsafe {
            NonNull::new_unchecked((*self.data.get()).as_ptr().add(offset).cast::<T>() as *mut T)
        })
    }

    pub(crate) fn len(&self) -> usize {
        self.len as usize
    }

    pub(crate) fn entities(&self) -> NonNull<u32> {
        unsafe { NonNull::new_unchecked(self.entities.as_ptr() as *mut _) }
    }

    pub(crate) fn entity_id(&self, index: u32) -> u32 {
        self.entities[index as usize]
    }

    pub(crate) fn types(&self) -> &[TypeInfo] {
        &self.types
    }

    /// `index` must be in-bounds
    pub(crate) unsafe fn get<T: Component>(&self, index: u32) -> Option<NonNull<T>> {
        debug_assert!(index < self.len);
        Some(NonNull::new_unchecked(
            self.data::<T>()?.as_ptr().add(index as usize),
        ))
    }

    /// `index` must be in-bounds
    pub(crate) unsafe fn get_dynamic(
        &self,
        ty: TypeId,
        size: usize,
        index: u32,
    ) -> Option<NonNull<u8>> {
        debug_assert!(index < self.len);
        Some(NonNull::new_unchecked(
            (*self.data.get())
                .as_mut_ptr()
                .add(*self.offsets.get(&ty)? + size * index as usize)
                .cast::<u8>(),
        ))
    }

    /// Every type must be written immediately after this call
    pub(crate) unsafe fn allocate(&mut self, id: u32) -> u32 {
        if (self.len as usize) < self.entities.len() {
            self.entities[self.len as usize] = id;
            self.len += 1;
            return self.len - 1;
        }

        // At this point we need to allocate more storage.
        let old_count = self.entities.len();
        let count = if old_count == 0 { 64 } else { old_count * 2 };
        let mut new_entities = vec![!0; count].into_boxed_slice();
        new_entities[0..old_count].copy_from_slice(&self.entities);
        self.entities = new_entities;

        let mut data_size = 0;
        let mut offsets = FxHashMap::with_capacity_and_hasher(self.types.len(), Default::default());
        for ty in &self.types {
            data_size = align(data_size, ty.layout.align());
            offsets.insert(ty.id, data_size);
            data_size += ty.layout.size() * count;
        }
        let raw = if data_size == 0 {
            Box::<[MaybeUninit<u8>]>::into_raw(Box::new([MaybeUninit::<u8>::uninit(); 0]))
        } else {
            let ptr = alloc(
                Layout::from_size_align(
                    data_size,
                    self.types.first().map_or(1, |x| x.layout.align()),
                )
                .unwrap(),
            )
            .cast::<MaybeUninit<u8>>();
            std::slice::from_raw_parts_mut(ptr, data_size)
        };
        let mut new_data = Box::from_raw(raw);
        if !(*self.data.get()).is_empty() {
            for ty in &self.types {
                let old_off = *self.offsets.get(&ty.id).unwrap();
                let new_off = *offsets.get(&ty.id).unwrap();
                ptr::copy_nonoverlapping(
                    (*self.data.get()).as_ptr().add(old_off),
                    new_data.as_mut_ptr().add(new_off),
                    ty.layout.size() * old_count,
                );
            }
        }

        self.data = UnsafeCell::new(new_data);
        self.offsets = offsets;
        self.entities[self.len as usize] = id;
        self.len += 1;
        self.len - 1
    }

    /// Returns the ID of the entity moved into `index`, if any
    pub(crate) unsafe fn remove(&mut self, index: u32) -> Option<u32> {
        let last = self.len - 1;
        for ty in &self.types {
            let removed = self
                .get_dynamic(ty.id, ty.layout.size(), index)
                .unwrap()
                .as_ptr();
            (ty.drop)(removed);
            if index != last {
                ptr::copy_nonoverlapping(
                    self.get_dynamic(ty.id, ty.layout.size(), last)
                        .unwrap()
                        .as_ptr(),
                    removed,
                    ty.layout.size(),
                );
            }
        }
        self.len = last;
        if index != last {
            self.entities[index as usize] = self.entities[last as usize];
            Some(self.entities[last as usize])
        } else {
            None
        }
    }

    pub(crate) unsafe fn move_to(&mut self, index: u32, mut f: impl FnMut(*mut u8, TypeId, usize)) {
        let last = self.len - 1;
        for ty in &self.types {
            let moved = self
                .get_dynamic(ty.id, ty.layout.size(), index)
                .unwrap()
                .as_ptr();
            // Unused fields were already moved out of
            f(moved, ty.id(), ty.layout().size());
            if index != last {
                ptr::copy_nonoverlapping(
                    self.get_dynamic(ty.id, ty.layout.size(), last)
                        .unwrap()
                        .as_ptr(),
                    moved,
                    ty.layout.size(),
                );
            }
        }
        if index != last {
            self.entities[index as usize] = self.entities[last as usize];
        }
        self.len -= 1;
    }

    pub(crate) unsafe fn put_dynamic(
        &mut self,
        component: *mut u8,
        ty: TypeId,
        size: usize,
        index: u32,
    ) {
        let ptr = self
            .get_dynamic(ty, size, index)
            .unwrap()
            .as_ptr()
            .cast::<u8>();
        ptr::copy_nonoverlapping(component, ptr, size);
    }
}

impl Drop for Archetype {
    fn drop(&mut self) {
        self.clear();
    }
}

fn align(x: usize, alignment: usize) -> usize {
    assert!(alignment.is_power_of_two());
    (x + alignment - 1) & (!alignment + 1)
}

/// Metadata required to store a component
#[derive(Debug, Copy, Clone)]
pub struct TypeInfo {
    id: TypeId,
    layout: Layout,
    drop: unsafe fn(*mut u8),
}

impl TypeInfo {
    /// Metadata for `T`
    pub fn of<T: 'static>() -> Self {
        unsafe fn drop_ptr<T>(x: *mut u8) {
            x.cast::<T>().drop_in_place()
        }

        Self {
            id: TypeId::of::<T>(),
            layout: Layout::new::<T>(),
            drop: drop_ptr::<T>,
        }
    }

    pub(crate) fn id(&self) -> TypeId {
        self.id
    }

    pub(crate) fn layout(&self) -> Layout {
        self.layout
    }

    pub(crate) unsafe fn drop(&self, data: *mut u8) {
        (self.drop)(data)
    }
}

impl PartialOrd for TypeInfo {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for TypeInfo {
    /// Order by alignment, descending. Ties broken with TypeId.
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.layout
            .align()
            .cmp(&other.layout.align())
            .reverse()
            .then_with(|| self.id.cmp(&other.id))
    }
}

impl PartialEq for TypeInfo {
    fn eq(&self, other: &Self) -> bool {
        self.id == other.id
    }
}

impl Eq for TypeInfo {}
