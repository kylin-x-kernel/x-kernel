// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

pub trait AddrOps: Copy + From<usize> + Into<usize> + Ord {
    #[inline]
    #[must_use = "this returns a new address, without modifying the original"]
    fn floor_align<U>(self, align: U) -> Self
    where
        U: Into<usize>,
    {
        Self::from(crate::floor_align(self.into(), align.into()))
    }

    #[inline]
    #[must_use = "this returns a new address, without modifying the original"]
    fn ceil_align<U>(self, align: U) -> Self
    where
        U: Into<usize>,
    {
        Self::from(crate::ceil_align(self.into(), align.into()))
    }

    #[inline]
    #[must_use = "this function has no side effects, so it can be removed if the return value is \
                  not used"]
    fn align_rem<U>(self, align: U) -> usize
    where
        U: Into<usize>,
    {
        crate::align_rem(self.into(), align.into())
    }

    #[inline]
    #[must_use = "this function has no side effects, so it can be removed if the return value is \
                  not used"]
    fn aligned_to<U>(self, align: U) -> bool
    where
        U: Into<usize>,
    {
        crate::aligned_to(self.into(), align.into())
    }

    #[inline]
    #[must_use = "this returns a new address, without modifying the original"]
    fn floor_4k(self) -> Self {
        Self::from(crate::floor_4k(self.into()))
    }

    #[inline]
    #[must_use = "this returns a new address, without modifying the original"]
    fn ceil_4k(self) -> Self {
        Self::from(crate::ceil_4k(self.into()))
    }

    #[inline]
    #[must_use = "this function has no side effects, so it can be removed if the return value is \
                  not used"]
    fn rem_4k(self) -> usize {
        crate::rem_4k(self.into())
    }

    #[inline]
    #[must_use = "this function has no side effects, so it can be removed if the return value is \
                  not used"]
    fn aligned_4k(self) -> bool {
        crate::aligned_4k(self.into())
    }

    #[inline]
    #[must_use = "this returns a new address, without modifying the original"]
    fn offset_signed(self, delta: isize) -> Self {
        let base: usize = self.into();
        let next =
            usize::checked_add_signed(base, delta).expect("overflow in `AddrOps::offset_signed`");
        Self::from(next)
    }

    #[inline]
    #[must_use = "this returns a new address, without modifying the original"]
    fn offset_wrapping(self, delta: isize) -> Self {
        Self::from(usize::wrapping_add_signed(self.into(), delta))
    }

    #[inline]
    #[must_use = "this function has no side effects, so it can be removed if the return value is \
                  not used"]
    fn delta_from(self, base: Self) -> isize {
        let current: usize = self.into();
        let origin: usize = base.into();
        let raw = current.wrapping_sub(origin) as isize;
        if raw == 0 {
            return 0;
        }
        let forward = base < self;
        if (raw > 0) == forward {
            raw
        } else {
            panic!("overflow in `AddrOps::delta_from`");
        }
    }

    #[inline]
    #[must_use = "this returns a new address, without modifying the original"]
    fn add_usize(self, rhs: usize) -> Self {
        let sum = usize::checked_add(self.into(), rhs).expect("overflow in `AddrOps::add_usize`");
        Self::from(sum)
    }

    #[inline]
    #[must_use = "this returns a new address, without modifying the original"]
    fn add_wrapping(self, rhs: usize) -> Self {
        Self::from(usize::wrapping_add(self.into(), rhs))
    }

    #[inline]
    #[must_use = "this returns a new address, without modifying the original"]
    fn add_overflowing(self, rhs: usize) -> (Self, bool) {
        let (value, carry) = self.into().overflowing_add(rhs);
        (Self::from(value), carry)
    }

    #[inline]
    #[must_use = "this returns a new address, without modifying the original"]
    fn add_checked(self, rhs: usize) -> Option<Self> {
        usize::checked_add(self.into(), rhs).map(Self::from)
    }

    #[inline]
    #[must_use = "this returns a new address, without modifying the original"]
    fn sub_usize(self, rhs: usize) -> Self {
        let value = usize::checked_sub(self.into(), rhs).expect("overflow in `AddrOps::sub_usize`");
        Self::from(value)
    }

    #[inline]
    #[must_use = "this returns a new address, without modifying the original"]
    fn sub_wrapping(self, rhs: usize) -> Self {
        Self::from(usize::wrapping_sub(self.into(), rhs))
    }

    #[inline]
    #[must_use = "this returns a new address, without modifying the original"]
    fn sub_overflowing(self, rhs: usize) -> (Self, bool) {
        let (value, borrow) = self.into().overflowing_sub(rhs);
        (Self::from(value), borrow)
    }

    #[inline]
    #[must_use = "this returns a new address, without modifying the original"]
    fn sub_checked(self, rhs: usize) -> Option<Self> {
        usize::checked_sub(self.into(), rhs).map(Self::from)
    }

    #[inline]
    #[must_use = "this function has no side effects, so it can be removed if the return value is \
                  not used"]
    fn diff(self, rhs: Self) -> usize {
        usize::checked_sub(self.into(), rhs.into()).expect("overflow in `AddrOps::diff`")
    }

    #[inline]
    #[must_use = "this function has no side effects, so it can be removed if the return value is \
                  not used"]
    fn diff_wrapping(self, rhs: Self) -> usize {
        usize::wrapping_sub(self.into(), rhs.into())
    }

    #[inline]
    #[must_use = "this function has no side effects, so it can be removed if the return value is \
                  not used"]
    fn diff_overflowing(self, rhs: Self) -> (usize, bool) {
        usize::overflowing_sub(self.into(), rhs.into())
    }

    #[inline]
    #[must_use = "this function has no side effects, so it can be removed if the return value is \
                  not used"]
    fn diff_checked(self, rhs: Self) -> Option<usize> {
        usize::checked_sub(self.into(), rhs.into())
    }
}

pub trait MemoryAddr: AddrOps {
    #[inline]
    #[must_use = "this returns a new address, without modifying the original"]
    fn align_down<U>(self, align: U) -> Self
    where
        U: Into<usize>,
    {
        self.floor_align(align)
    }

    #[inline]
    #[must_use = "this returns a new address, without modifying the original"]
    fn align_up<U>(self, align: U) -> Self
    where
        U: Into<usize>,
    {
        self.ceil_align(align)
    }

    #[inline]
    #[must_use = "this function has no side effects, so it can be removed if the return value is \
                  not used"]
    fn align_offset<U>(self, align: U) -> usize
    where
        U: Into<usize>,
    {
        self.align_rem(align)
    }

    #[inline]
    #[must_use = "this function has no side effects, so it can be removed if the return value is \
                  not used"]
    fn is_aligned<U>(self, align: U) -> bool
    where
        U: Into<usize>,
    {
        self.aligned_to(align)
    }

    #[inline]
    #[must_use = "this returns a new address, without modifying the original"]
    fn align_down_4k(self) -> Self {
        self.floor_4k()
    }

    #[inline]
    #[must_use = "this returns a new address, without modifying the original"]
    fn align_up_4k(self) -> Self {
        self.ceil_4k()
    }

    #[inline]
    #[must_use = "this function has no side effects, so it can be removed if the return value is \
                  not used"]
    fn align_offset_4k(self) -> usize {
        self.rem_4k()
    }

    #[inline]
    #[must_use = "this function has no side effects, so it can be removed if the return value is \
                  not used"]
    fn is_aligned_4k(self) -> bool {
        self.aligned_4k()
    }

    #[inline]
    #[must_use = "this returns a new address, without modifying the original"]
    fn offset(self, offset: isize) -> Self {
        self.offset_signed(offset)
    }

    #[inline]
    #[must_use = "this returns a new address, without modifying the original"]
    fn wrapping_offset(self, offset: isize) -> Self {
        self.offset_wrapping(offset)
    }

    #[inline]
    #[must_use = "this function has no side effects, so it can be removed if the return value is \
                  not used"]
    fn offset_from(self, base: Self) -> isize {
        self.delta_from(base)
    }

    #[inline]
    #[must_use = "this returns a new address, without modifying the original"]
    fn add(self, rhs: usize) -> Self {
        self.add_usize(rhs)
    }

    #[inline]
    #[must_use = "this returns a new address, without modifying the original"]
    fn wrapping_add(self, rhs: usize) -> Self {
        self.add_wrapping(rhs)
    }

    #[inline]
    #[must_use = "this returns a new address, without modifying the original"]
    fn overflowing_add(self, rhs: usize) -> (Self, bool) {
        self.add_overflowing(rhs)
    }

    #[inline]
    #[must_use = "this returns a new address, without modifying the original"]
    fn checked_add(self, rhs: usize) -> Option<Self> {
        self.add_checked(rhs)
    }

    #[inline]
    #[must_use = "this returns a new address, without modifying the original"]
    fn sub(self, rhs: usize) -> Self {
        self.sub_usize(rhs)
    }

    #[inline]
    #[must_use = "this returns a new address, without modifying the original"]
    fn wrapping_sub(self, rhs: usize) -> Self {
        self.sub_wrapping(rhs)
    }

    #[inline]
    #[must_use = "this returns a new address, without modifying the original"]
    fn overflowing_sub(self, rhs: usize) -> (Self, bool) {
        self.sub_overflowing(rhs)
    }

    #[inline]
    #[must_use = "this returns a new address, without modifying the original"]
    fn checked_sub(self, rhs: usize) -> Option<Self> {
        self.sub_checked(rhs)
    }

    #[inline]
    #[must_use = "this function has no side effects, so it can be removed if the return value is \
                  not used"]
    fn sub_addr(self, rhs: Self) -> usize {
        self.diff(rhs)
    }

    #[inline]
    #[must_use = "this function has no side effects, so it can be removed if the return value is \
                  not used"]
    fn wrapping_sub_addr(self, rhs: Self) -> usize {
        self.diff_wrapping(rhs)
    }

    #[inline]
    #[must_use = "this function has no side effects, so it can be removed if the return value is \
                  not used"]
    fn overflowing_sub_addr(self, rhs: Self) -> (usize, bool) {
        self.diff_overflowing(rhs)
    }

    #[inline]
    #[must_use = "this function has no side effects, so it can be removed if the return value is \
                  not used"]
    fn checked_sub_addr(self, rhs: Self) -> Option<usize> {
        self.diff_checked(rhs)
    }
}

impl<T> AddrOps for T where T: Copy + From<usize> + Into<usize> + Ord {}

impl<T> MemoryAddr for T where T: Copy + From<usize> + Into<usize> + Ord {}

#[macro_export]
macro_rules! def_usize_addr {
    (
        $(#[$meta:meta])*
        $vis:vis type $name:ident;

        $($tt:tt)*
    ) => {
        #[repr(transparent)]
        #[derive(Copy, Clone, Default, Ord, PartialOrd, Eq, PartialEq)]
        $(#[$meta])*
        pub struct $name(usize);

        impl $name {
            #[inline]
            pub const fn from_raw(addr: usize) -> Self {
                Self(addr)
            }

            #[inline]
            pub const fn raw(self) -> usize {
                self.0
            }

            #[inline]
            pub const fn from_usize(addr: usize) -> Self {
                Self::from_raw(addr)
            }

            #[inline]
            pub const fn as_usize(self) -> usize {
                self.raw()
            }
        }

        impl From<usize> for $name {
            #[inline]
            fn from(addr: usize) -> Self {
                Self::from_raw(addr)
            }
        }

        impl From<$name> for usize {
            #[inline]
            fn from(addr: $name) -> usize {
                addr.raw()
            }
        }

        impl core::ops::Add<usize> for $name {
            type Output = Self;
            #[inline]
            fn add(self, rhs: usize) -> Self {
                let sum = self.0 + rhs;
                Self(sum)
            }
        }

        impl core::ops::AddAssign<usize> for $name {
            #[inline]
            fn add_assign(&mut self, rhs: usize) {
                self.0 += rhs;
            }
        }

        impl core::ops::Sub<usize> for $name {
            type Output = Self;
            #[inline]
            fn sub(self, rhs: usize) -> Self {
                let diff = self.0 - rhs;
                Self(diff)
            }
        }

        impl core::ops::SubAssign<usize> for $name {
            #[inline]
            fn sub_assign(&mut self, rhs: usize) {
                self.0 -= rhs;
            }
        }

        impl core::ops::Sub<$name> for $name {
            type Output = usize;
            #[inline]
            fn sub(self, rhs: $name) -> usize {
                self.0 - rhs.0
            }
        }

        $crate::def_usize_addr!($($tt)*);
    };
    () => {};
}

#[macro_export]
macro_rules! def_usize_addr_formatter {
    (
        $name:ident = $format:literal;

        $($tt:tt)*
    ) => {
        impl core::fmt::Debug for $name {
            fn fmt(&self, f: &mut core::fmt::Formatter) -> core::fmt::Result {
                let inner = format_args!("{:#x}", self.0);
                f.write_fmt(format_args!($format, inner))
            }
        }

        impl core::fmt::LowerHex for $name {
            fn fmt(&self, f: &mut core::fmt::Formatter) -> core::fmt::Result {
                let inner = format_args!("{:#x}", self.0);
                f.write_fmt(format_args!($format, inner))
            }
        }

        impl core::fmt::UpperHex for $name {
            fn fmt(&self, f: &mut core::fmt::Formatter) -> core::fmt::Result {
                let inner = format_args!("{:#X}", self.0);
                f.write_fmt(format_args!($format, inner))
            }
        }

        $crate::def_usize_addr_formatter!($($tt)*);
    };
    () => {};
}

def_usize_addr! {
    pub type PhysAddr;
    pub type VirtAddr;
}

def_usize_addr_formatter! {
    PhysAddr = "PA:{}";
    VirtAddr = "VA:{}";
}

impl VirtAddr {
    #[inline]
    pub fn from_ptr<T>(ptr: *const T) -> Self {
        Self(ptr as usize)
    }

    #[inline]
    pub fn from_mut_ptr<T>(ptr: *mut T) -> Self {
        Self(ptr as usize)
    }

    #[inline]
    pub const fn ptr_u8(self) -> *const u8 {
        self.0 as *const u8
    }

    #[inline]
    pub const fn ptr_of<T>(self) -> *const T {
        self.0 as *const T
    }

    #[inline]
    pub const fn mut_ptr_u8(self) -> *mut u8 {
        self.0 as *mut u8
    }

    #[inline]
    pub const fn mut_ptr_of<T>(self) -> *mut T {
        self.0 as *mut T
    }

    #[inline]
    pub fn from_ptr_of<T>(ptr: *const T) -> Self {
        Self::from_ptr(ptr)
    }

    #[inline]
    pub fn from_mut_ptr_of<T>(ptr: *mut T) -> Self {
        Self::from_mut_ptr(ptr)
    }

    #[inline]
    pub const fn as_ptr(self) -> *const u8 {
        self.ptr_u8()
    }

    #[inline]
    pub const fn as_ptr_of<T>(self) -> *const T {
        self.ptr_of()
    }

    #[inline]
    pub const fn as_mut_ptr(self) -> *mut u8 {
        self.mut_ptr_u8()
    }

    #[inline]
    pub const fn as_mut_ptr_of<T>(self) -> *mut T {
        self.mut_ptr_of()
    }
}

#[macro_export]
macro_rules! pa {
    ($addr:expr) => {
        $crate::PhysAddr::from_usize($addr)
    };
}

#[macro_export]
macro_rules! va {
    ($addr:expr) => {
        $crate::VirtAddr::from_usize($addr)
    };
}
