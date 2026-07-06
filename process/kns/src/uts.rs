// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! UTS namespace.

use core::ffi::c_char;

use ksync::RwLock;

use crate::{error::UtsError, types::NamespaceId};

const UTS_LEN: usize = 65;

const DEFAULT_NODENAME: &[u8] = b"kylin-x";
const DEFAULT_DOMAINNAME: &[u8] = b"https://gitee/openkylin/x-kernel";

fn zero_uts_buf() -> [c_char; UTS_LEN] {
    [0; UTS_LEN]
}

/// Reinterprets a `c_char` slice as a `u8` slice up to the first NUL byte.
///
/// `c_char` is `i8` on some targets and `u8` on others, but it always has the
/// same size and alignment as `u8`, and the values we store are 7-bit ASCII
/// hostnames/domainnames, so a byte-level view is well-defined.
fn bytes_from_uts(buf: &[c_char]) -> &[u8] {
    let len = buf.iter().position(|&c| c == 0).unwrap_or(UTS_LEN);
    // SAFETY: `c_char` is either `i8` or `u8` depending on the target, but in
    // both cases it is one byte with the same alignment as `u8`. `buf` is a
    // valid borrowed slice, and `len` is bounded by `buf.len()` (UTS_LEN), so
    // the resulting slice stays within the original allocation. The stored
    // values are ASCII bytes, which are representable in both signed and
    // unsigned `c_char`.
    unsafe { core::slice::from_raw_parts(buf.as_ptr().cast::<u8>(), len) }
}

/// Copies `src` bytes into a `c_char` destination, element by element.
///
/// This avoids `transmute`, which would be UB on targets where `c_char` is
/// `i8` (it changes signedness and violates strict aliasing). ASCII bytes are
/// representable in both signed and unsigned `c_char`.
fn copy_bytes_to_uts(dst: &mut [c_char], src: &[u8]) {
    for (slot, &byte) in dst.iter_mut().zip(src.iter()) {
        *slot = byte as c_char;
    }
}

/// Mutable state of a UTS namespace.
pub struct UtsInner {
    nodename: [c_char; UTS_LEN],
    domainname: [c_char; UTS_LEN],
}

impl Default for UtsInner {
    fn default() -> Self {
        let mut nodename = zero_uts_buf();
        let mut domainname = zero_uts_buf();
        let len = DEFAULT_NODENAME.len().min(UTS_LEN - 1);
        copy_bytes_to_uts(&mut nodename, &DEFAULT_NODENAME[..len]);
        let len = DEFAULT_DOMAINNAME.len().min(UTS_LEN - 1);
        copy_bytes_to_uts(&mut domainname, &DEFAULT_DOMAINNAME[..len]);
        Self {
            nodename,
            domainname,
        }
    }
}

impl UtsInner {
    /// Returns the nodename (hostname) as a byte slice.
    pub fn nodename(&self) -> &[u8] {
        bytes_from_uts(&self.nodename)
    }

    /// Sets the nodename (hostname) from a byte slice.
    ///
    /// Returns an error if the name exceeds 64 bytes.
    pub fn set_nodename(&mut self, name: &[u8]) -> Result<(), UtsError> {
        if name.len() >= UTS_LEN {
            return Err(UtsError::NameTooLong);
        }
        self.nodename = zero_uts_buf();
        copy_bytes_to_uts(&mut self.nodename, name);
        Ok(())
    }

    /// Returns the domainname as a byte slice.
    pub fn domainname(&self) -> &[u8] {
        bytes_from_uts(&self.domainname)
    }

    /// Sets the domainname from a byte slice.
    ///
    /// Returns an error if the name exceeds 64 bytes.
    pub fn set_domainname(&mut self, name: &[u8]) -> Result<(), UtsError> {
        if name.len() >= UTS_LEN {
            return Err(UtsError::NameTooLong);
        }
        self.domainname = zero_uts_buf();
        copy_bytes_to_uts(&mut self.domainname, name);
        Ok(())
    }
}

/// UTS namespace.
///
/// Holds hostname (nodename) and domainname, which are per-namespace mutable.
/// Other uname fields (sysname, release, version, machine) remain global kernel constants.
pub struct UtsNamespace {
    id: NamespaceId,
    inner: RwLock<UtsInner>,
}

impl Default for UtsNamespace {
    fn default() -> Self {
        Self::new()
    }
}

impl UtsNamespace {
    /// Creates a new UTS namespace with default hostname and domainname.
    pub fn new() -> Self {
        Self {
            id: NamespaceId::new(),
            inner: RwLock::new(UtsInner::default()),
        }
    }

    /// Creates a new UTS namespace with the same hostname/domainname as the source.
    pub fn clone_from(source: &UtsNamespace) -> Self {
        let inner = source.inner.read();
        Self {
            id: NamespaceId::new(),
            inner: RwLock::new(UtsInner {
                nodename: inner.nodename,
                domainname: inner.domainname,
            }),
        }
    }

    /// Returns the namespace ID.
    pub fn id(&self) -> NamespaceId {
        self.id
    }

    /// Reads the current nodename.
    pub fn nodename(&self) -> alloc::vec::Vec<u8> {
        self.inner.read().nodename().to_vec()
    }

    /// Reads the current domainname.
    pub fn domainname(&self) -> alloc::vec::Vec<u8> {
        self.inner.read().domainname().to_vec()
    }

    /// Copies the current nodename and domainname into stack buffers in a
    /// single locked read, avoiding the two heap allocations of
    /// [`Self::nodename`] / [`Self::domainname`].
    ///
    /// Intended for hot paths such as `uname()`, where the caller immediately
    /// copies the bytes into a fixed-size struct and discards the `Vec`.
    pub fn read_names_into(
        &self,
        nodename: &mut [c_char; UTS_LEN],
        domainname: &mut [c_char; UTS_LEN],
    ) {
        let inner = self.inner.read();
        *nodename = inner.nodename;
        *domainname = inner.domainname;
    }

    /// Sets the nodename (hostname).
    pub fn set_nodename(&self, name: &[u8]) -> Result<(), UtsError> {
        self.inner.write().set_nodename(name)
    }

    /// Sets the domainname.
    pub fn set_domainname(&self, name: &[u8]) -> Result<(), UtsError> {
        self.inner.write().set_domainname(name)
    }
}

#[cfg(unittest)]
mod tests_uts {
    use unittest::def_test;

    use super::*;

    #[def_test]
    fn test_uts_namespace_default_values() {
        let ns = UtsNamespace::new();
        assert_eq!(ns.nodename(), b"kylin-x");
        assert_eq!(ns.domainname(), b"https://gitee/openkylin/x-kernel");
    }

    #[def_test]
    fn test_uts_namespace_set_nodename() {
        let ns = UtsNamespace::new();
        ns.set_nodename(b"testhost").unwrap();
        assert_eq!(ns.nodename(), b"testhost");
    }

    #[def_test]
    fn test_uts_namespace_set_domainname() {
        let ns = UtsNamespace::new();
        ns.set_domainname(b"example.com").unwrap();
        assert_eq!(ns.domainname(), b"example.com");
    }

    #[def_test]
    fn test_uts_namespace_rejects_oversized_name() {
        let ns = UtsNamespace::new();
        let long_name = [b'x'; 65];
        assert!(ns.set_nodename(&long_name).is_err());
        assert!(ns.set_domainname(&long_name).is_err());
    }

    #[def_test]
    fn test_uts_namespace_clone_isolation() {
        let original = UtsNamespace::new();
        original.set_nodename(b"parent").unwrap();

        let cloned = UtsNamespace::clone_from(&original);
        cloned.set_nodename(b"child").unwrap();

        assert_eq!(original.nodename(), b"parent");
        assert_eq!(cloned.nodename(), b"child");
    }

    #[def_test]
    fn test_uts_namespace_unique_ids() {
        let ns1 = UtsNamespace::new();
        let ns2 = UtsNamespace::new();
        assert_ne!(ns1.id(), ns2.id());
    }
}
