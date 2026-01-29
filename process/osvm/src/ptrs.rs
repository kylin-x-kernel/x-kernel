use core::{mem::MaybeUninit, ptr::NonNull, slice};

use bytemuck::AnyBitPattern;

use crate::{MemResult, read_vm_mem, write_vm_mem};

pub trait VirtPtr: Copy {
    type Target;

    fn as_ptr(self) -> *const Self::Target;

    fn check_non_null(self) -> Option<Self> {
        if self.as_ptr().is_null() {
            None
        } else {
            Some(self)
        }
    }

    fn read_uninit(self) -> MemResult<MaybeUninit<Self::Target>> {
        let mut u = MaybeUninit::<Self::Target>::uninit();
        read_vm_mem(self.as_ptr(), slice::from_mut(&mut u))?;
        Ok(u)
    }

    fn read_vm(self) -> MemResult<Self::Target>
    where
        Self::Target: AnyBitPattern,
    {
        let u = self.read_uninit()?;
        Ok(unsafe { u.assume_init() })
    }
}

impl<T> VirtPtr for *const T {
    type Target = T;

    fn as_ptr(self) -> *const T {
        self
    }
}

impl<T> VirtPtr for *mut T {
    type Target = T;

    fn as_ptr(self) -> *const T {
        self
    }
}

impl<T> VirtPtr for NonNull<T> {
    type Target = T;

    fn as_ptr(self) -> *const T {
        self.as_ptr()
    }
}

pub trait VirtMutPtr: VirtPtr {
    fn write_vm(self, v: Self::Target) -> MemResult {
        write_vm_mem(self.as_ptr().cast_mut(), slice::from_ref(&v))
    }
}

impl<T> VirtMutPtr for *mut T {}
impl<T> VirtMutPtr for NonNull<T> {}
