// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

#[macro_export]
macro_rules! impl_pte_debug {
    ($struct_name:ident) => {
        impl core::fmt::Debug for $struct_name {
            fn fmt(&self, f: &mut core::fmt::Formatter) -> core::fmt::Result {
                f.debug_struct(stringify!($struct_name))
                    .field("paddr", &self.paddr())
                    .field("flags", &self.flags())
                    .finish()
            }
        }
    };
}

#[macro_export]
macro_rules! impl_pte_common_ops {
    ($flags_ty:ty, $paddr_mask:expr) => {
        const EMPTY: Self = Self(0);

        fn paddr(&self) -> PhysAddr {
            PhysAddr::from((self.0 & $paddr_mask) as usize)
        }

        fn flags(&self) -> PagingFlags {
            <$flags_ty>::from_bits_truncate(self.0).into()
        }

        fn set_paddr(&mut self, paddr: PhysAddr) {
            self.0 = (self.0 & !$paddr_mask) | (paddr.as_usize() as u64 & $paddr_mask);
        }

        fn bits(self) -> usize {
            self.0 as usize
        }
    };
}

#[macro_export]
macro_rules! walk_page_table {
    ($self:expr, $vaddr:expr, $table_fn:ident, $next_fn:ident, $is_mut:ident) => {{
        let vaddr: usize = $vaddr.into();
        let p3 = if M::LEVELS == 3 {
            $crate::walk_page_table!(@call $self, $table_fn, $self.root_paddr(), $is_mut)
        } else if M::LEVELS == 4 {
            let p4 = $crate::walk_page_table!(@call $self, $table_fn, $self.root_paddr(), $is_mut);
            $crate::walk_page_table!(@call_next $self, $next_fn, p4, p4_idx(vaddr), $is_mut)?
        } else {
            unreachable!()
        };
        let p3e_is_huge = $crate::walk_page_table!(@is_huge p3, p3_idx(vaddr));
        if p3e_is_huge {
            return Ok(($crate::walk_page_table!(@get p3, p3_idx(vaddr), $is_mut), PageSize::Size1G));
        }

        let p2 = $crate::walk_page_table!(@call_next $self, $next_fn, p3, p3_idx(vaddr), $is_mut)?;
        let p2e_is_huge = $crate::walk_page_table!(@is_huge p2, p2_idx(vaddr));
        if p2e_is_huge {
            return Ok(($crate::walk_page_table!(@get p2, p2_idx(vaddr), $is_mut), PageSize::Size2M));
        }

        let p1 = $crate::walk_page_table!(@call_next $self, $next_fn, p2, p2_idx(vaddr), $is_mut)?;
        Ok(($crate::walk_page_table!(@get p1, p1_idx(vaddr), $is_mut), PageSize::Size4K))
    }};
    (@call $self:expr, $fn:ident, $arg:expr, mut) => { $self.$fn($arg) };
    (@call $self:expr, $fn:ident, $arg:expr, ref) => { $self.$fn($arg) };
    (@call_next $self:expr, $fn:ident, $table:expr, $idx:expr, mut) => { $self.$fn(&mut $table[$idx]) };
    (@call_next $self:expr, $fn:ident, $table:expr, $idx:expr, ref) => { $self.$fn(&$table[$idx]) };
    (@get $table:expr, $idx:expr, mut) => { &mut $table[$idx] };
    (@get $table:expr, $idx:expr, ref) => { &$table[$idx] };
    (@is_huge $table:expr, $idx:expr) => { $table[$idx].is_huge() };
}

#[macro_export]
macro_rules! walk_page_table_create {
    ($self:expr, $vaddr:expr, $page_size:expr) => {{
        let vaddr: usize = $vaddr.into();
        let p3 = if M::LEVELS == 3 {
            $self.table_of_mut($self.root_paddr())
        } else if M::LEVELS == 4 {
            let p4 = $self.table_of_mut($self.root_paddr());
            let p4e = &mut p4[p4_idx(vaddr)];
            $self.next_table_mut_or_create(p4e)?
        } else {
            unreachable!()
        };
        let p3e = &mut p3[p3_idx(vaddr)];
        if $page_size == PageSize::Size1G {
            return Ok(p3e);
        }

        let p2 = $self.next_table_mut_or_create(p3e)?;
        let p2e = &mut p2[p2_idx(vaddr)];
        if $page_size == PageSize::Size2M {
            return Ok(p2e);
        }

        let p1 = $self.next_table_mut_or_create(p2e)?;
        let p1e = &mut p1[p1_idx(vaddr)];
        Ok(p1e)
    }};
}
