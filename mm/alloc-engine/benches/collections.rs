// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

#![feature(allocator_api)]
#![feature(btreemap_alloc)]

mod utils;

use std::{alloc::Allocator, collections::BTreeMap, io::Write};

use alloc_engine::{AllocatorRc, BuddyByteAllocator, SlabByteAllocator, TlsfByteAllocator};
use criterion::{Criterion, black_box, criterion_group, criterion_main};
use rand::{RngCore, SeedableRng, rngs::SmallRng, seq::SliceRandom};

use self::utils::MemoryPool;

const POOL_SIZE: usize = 1024 * 1024 * 128;

fn vec_push(n: usize, alloc: &(impl Allocator + Clone)) {
    let mut v: Vec<u32, _> = Vec::new_in(alloc.clone());
    #[allow(clippy::same_item_push)]
    for _ in 0..n {
        v.push(0xdead_beef);
    }
    drop(v);
}

fn vec_rand_free(n: usize, blk_size: usize, alloc: &(impl Allocator + Clone)) {
    let mut v = Vec::new_in(alloc.clone());
    for _ in 0..n {
        let block = Vec::<u64, _>::with_capacity_in(blk_size, alloc.clone());
        v.push(block);
    }

    let mut rng = SmallRng::seed_from_u64(0xdead_beef);
    let mut index = Vec::with_capacity_in(n, alloc.clone());
    for i in 0..n {
        index.push(i);
    }
    index.shuffle(&mut rng);

    for i in index {
        v[i] = Vec::new_in(alloc.clone());
    }
    drop(v);
}

fn btree_map(n: usize, alloc: &(impl Allocator + Clone)) {
    let mut rng = SmallRng::seed_from_u64(0xdead_beef);
    let mut m = BTreeMap::new_in(alloc.clone());
    for _ in 0..n {
        if rng.next_u32() % 5 == 0 && !m.is_empty() {
            m.pop_first();
        } else {
            let value = rng.next_u32();
            let mut key = Vec::new_in(alloc.clone());
            write!(&mut key, "key_{value}").unwrap();
            m.insert(key, value);
        }
    }
    m.clear();
    drop(m);
}

fn bench(c: &mut Criterion, alloc_name: &str, alloc: impl Allocator + Clone) {
    let mut g = c.benchmark_group(alloc_name);
    g.bench_function("vec_push_3M", |b| {
        b.iter(|| vec_push(black_box(3_000_000), &alloc));
    });
    g.sample_size(10);
    g.bench_function("vec_rand_free_25K_64", |b| {
        b.iter(|| vec_rand_free(black_box(25_000), black_box(64), &alloc));
    });
    g.bench_function("vec_rand_free_7500_520", |b| {
        b.iter(|| vec_rand_free(black_box(7_500), black_box(520), &alloc));
    });
    g.bench_function("btree_map_50K", |b| {
        b.iter(|| btree_map(black_box(50_000), &alloc));
    });
}

fn criterion_benchmark(c: &mut Criterion) {
    let mut pool = MemoryPool::new(POOL_SIZE);
    bench(c, "system", std::alloc::System);
    bench(
        c,
        "tlsf",
        AllocatorRc::new(TlsfByteAllocator::new(), pool.as_slice()),
    );
    bench(
        c,
        "slab",
        AllocatorRc::new(SlabByteAllocator::new(), pool.as_slice()),
    );
    bench(
        c,
        "buddy",
        AllocatorRc::new(BuddyByteAllocator::new(), pool.as_slice()),
    );
}

criterion_group!(benches, criterion_benchmark);
criterion_main!(benches);

mod bench_impls {
    use core::{alloc::Layout, ptr::NonNull};

    use buddy_slab_allocator::{
        SlabPoolTrait, SlabTrait,
        eii::{slab_pool_impl, virt_to_phys_impl},
    };
    #[virt_to_phys_impl]
    fn dummy_virt_to_phys(vaddr: usize) -> usize {
        vaddr
    }
    struct DummySlabPool;
    impl SlabTrait for DummySlabPool {
        fn cpu_id(&self) -> usize {
            0
        }

        fn page_size(&self) -> usize {
            4096
        }

        fn alloc(
            &self,
            _layout: Layout,
        ) -> buddy_slab_allocator::AllocResult<buddy_slab_allocator::SlabAllocResult> {
            Err(buddy_slab_allocator::AllocError::NoMemory)
        }

        fn add_slab(
            &self,
            _size_class: buddy_slab_allocator::SizeClass,
            _base: usize,
            _bytes: usize,
        ) {
        }

        fn dealloc_local(
            &self,
            _ptr: NonNull<u8>,
            _layout: Layout,
        ) -> buddy_slab_allocator::SlabDeallocResult {
            buddy_slab_allocator::SlabDeallocResult::Done
        }
    }
    static DUMMY_POOL: DummySlabPool = DummySlabPool;
    impl SlabPoolTrait for DummySlabPool {
        fn current_slab(&self) -> &dyn SlabTrait {
            &DUMMY_POOL
        }

        fn owner_slab(&self, _cpu_idx: usize) -> &dyn SlabTrait {
            &DUMMY_POOL
        }
    }
    #[slab_pool_impl]
    fn dummy_slab_pool() -> &'static dyn SlabPoolTrait {
        &DUMMY_POOL
    }
}
