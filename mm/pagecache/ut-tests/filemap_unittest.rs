// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use memaddr::PAGE_SIZE_4K;
use unittest::def_test;

use super::*;

#[def_test]
fn collected_folios_reject_removed_and_uninitialized_pages() {
    let cache = MappingPages::new();
    let count = FolioBatch::CAPACITY + 2;
    for index in 0..count {
        let mut folio = Folio::new_zeroed(index as u64).unwrap();
        folio.data()[..3].copy_from_slice(b"old");
        assert!(cache.filemap_add_folio(folio).is_ok());
    }
    let (batch, run_len) = cache.get_folios_contig_with_run_len(0, count);
    assert_eq!(batch.len(), FolioBatch::CAPACITY);
    assert_eq!(run_len, count);
    cache.resize_cached_folios((count * PAGE_SIZE_4K) as u64, 0);
    let mut replacement = Folio::new_zeroed(0).unwrap();
    replacement.data()[..3].copy_from_slice(b"new");
    assert!(cache.filemap_add_folio(replacement).is_ok());
    assert_eq!(batch.with_read_folio(0, |_| ()), None);
    assert_eq!(
        cache
            .get_folios_contig(0, 1)
            .with_read_folio(0, |folio| folio.data()[..3].to_vec()),
        Some(b"new".to_vec())
    );
    assert_eq!(
        cache.with_folio_or_create(1, |_| Err(kerrno::KError::Io), |_| Ok(())),
        Err(kerrno::KError::Io)
    );
    assert_eq!(
        cache.get_folios_contig(1, 1).with_read_folio(0, |_| ()),
        None
    );
    cache
        .with_folio_or_create(
            1,
            |folio| {
                folio.data().fill(0x55);
                Ok(())
            },
            |_| Ok(()),
        )
        .unwrap();
    assert_eq!(
        cache
            .get_folios_contig(1, 1)
            .with_read_folio(0, |folio| folio.data()[0]),
        Some(0x55)
    );
}
