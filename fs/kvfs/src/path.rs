// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Pathname parsing and normalization utilities.
use alloc::{borrow::ToOwned, string::String};
use core::fmt;

/// Current directory component.
pub const DOT: &str = ".";
/// Parent directory component.
pub const DOTDOT: &str = "..";

/// Maximum filename length.
pub const MAX_NAME_LEN: usize = 255;

/// chars in a path name including nul
pub const PATH_MAX: usize = 4096;

/// A borrowed pathname view.
///
/// Different from [`std::path::Path`], this type is always UTF-8 encoded.
#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq, PartialOrd, Ord)]
pub struct Pathname<'a> {
    inner: &'a str,
}

impl<'a> Pathname<'a> {
    pub const fn new(s: &'a str) -> Self {
        Self { inner: s }
    }

    pub const fn as_str(&self) -> &'a str {
        self.inner
    }

    /// Returns the final component of the pathname, if there is one.
    pub fn file_name(&self) -> Option<&'a str> {
        let mut end = self.inner.len();
        loop {
            while end > 0 && self.inner.as_bytes()[end - 1] == b'/' {
                end -= 1;
            }
            if end == 0 {
                return None;
            }
            let start = self.inner[..end]
                .rfind('/')
                .map(|index| index + 1)
                .unwrap_or(0);
            let name = &self.inner[start..end];
            if name == "." && start > 0 {
                end = if start == 1 { 0 } else { start - 1 };
                continue;
            }
            return match name {
                "." | ".." => None,
                _ => Some(name),
            };
        }
    }

    /// Returns the pathname without its final component, if there is one.
    pub fn parent(&self) -> Option<Pathname<'a>> {
        let mut end = self.inner.len();
        loop {
            while end > 0 && self.inner.as_bytes()[end - 1] == b'/' {
                end -= 1;
            }
            if end == 0 {
                return None;
            }
            let start = self.inner[..end]
                .rfind('/')
                .map(|index| index + 1)
                .unwrap_or(0);
            let name = &self.inner[start..end];
            if name == "." && start > 0 {
                end = if start == 1 { 0 } else { start - 1 };
                continue;
            }
            let parent_end = if start == 0 {
                0
            } else if start == 1 {
                1
            } else {
                start - 1
            };
            return Some(Pathname::new(&self.inner[..parent_end]));
        }
    }

    /// Returns `true` if the pathname is absolute, i.e., if it is independent of
    /// the current directory.
    pub fn is_absolute(&self) -> bool {
        self.inner.starts_with('/')
    }

    /// Normalizes a path without performing I/O.
    pub fn normalize(&self) -> Option<PathBuf> {
        let mut ret = PathBuf::new();
        let mut rest = self.inner;
        let mut at_start = true;
        while !rest.is_empty() {
            let (component, next) = match rest.find('/') {
                Some(index) => (&rest[..index], &rest[index + 1..]),
                None => (rest, ""),
            };
            rest = next;
            match component {
                "" if at_start => {
                    ret.push("/");
                }
                "" => {}
                "." => {}
                ".." => {
                    if !ret.pop() {
                        return None;
                    }
                }
                name => {
                    ret.push(name);
                }
            }
            at_start = false;
        }
        Some(ret)
    }

    fn to_path_buf(self) -> PathBuf {
        PathBuf {
            inner: self.inner.to_owned(),
        }
    }
}

impl fmt::Display for Pathname<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.inner.fmt(f)
    }
}

impl<'a> From<&'a str> for Pathname<'a> {
    fn from(value: &'a str) -> Self {
        Pathname::new(value)
    }
}

impl AsRef<str> for Pathname<'_> {
    #[inline]
    fn as_ref(&self) -> &str {
        self.inner
    }
}

/// An owned, mutable [`Pathname`] buffer.
///
/// Different from [`std::path::PathBuf`], this type is always
/// UTF-8 encoded.
#[derive(Clone, Debug, Default, Hash, PartialEq, Eq, PartialOrd, Ord)]
pub struct PathBuf {
    inner: String,
}

impl PathBuf {
    pub const fn new() -> Self {
        Self {
            inner: String::new(),
        }
    }

    pub fn pop(&mut self) -> bool {
        match self.as_pathname().parent().map(|p| p.as_str().len()) {
            Some(len) => {
                self.inner.truncate(len);
                true
            }
            None => false,
        }
    }

    pub fn push(&mut self, path: impl AsRef<str>) {
        self._push(Pathname::new(path.as_ref()));
    }

    fn _push(&mut self, path: Pathname<'_>) {
        if path.as_str().is_empty() {
            return;
        }
        if path.is_absolute() {
            self.inner.clear();
        } else if !self.inner.ends_with('/') {
            self.inner.push('/');
        }
        self.inner += path.as_str();
    }

    pub fn as_str(&self) -> &str {
        &self.inner
    }

    pub fn as_pathname(&self) -> Pathname<'_> {
        Pathname::new(&self.inner)
    }

    pub fn is_absolute(&self) -> bool {
        self.as_pathname().is_absolute()
    }
}

impl<T: AsRef<str>> FromIterator<T> for PathBuf {
    fn from_iter<I: IntoIterator<Item = T>>(iter: I) -> Self {
        let mut path = PathBuf::new();
        for item in iter {
            path.push(item);
        }
        path
    }
}

impl AsRef<str> for PathBuf {
    #[inline]
    fn as_ref(&self) -> &str {
        &self.inner[..]
    }
}

impl From<String> for PathBuf {
    fn from(value: String) -> Self {
        Self { inner: value }
    }
}

impl From<&str> for PathBuf {
    fn from(value: &str) -> Self {
        Self {
            inner: value.to_owned(),
        }
    }
}

impl From<Pathname<'_>> for PathBuf {
    fn from(value: Pathname<'_>) -> Self {
        value.to_path_buf()
    }
}

impl fmt::Display for PathBuf {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.inner.fmt(f)
    }
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn test_file_name() {
        assert_eq!(Some("c"), Pathname::new("../a/b/c").file_name());
        assert_eq!(Some("b"), Pathname::new("a/b/.").file_name());
        assert_eq!(None, Pathname::new("a/..").file_name());
    }
}

#[cfg(unittest)]
mod tests {
    use unittest::{assert, assert_eq, def_test};

    use super::*;

    #[def_test]
    fn test_path_normalization() {
        let path = PathBuf::from("/path/to/file");
        assert_eq!(path.as_str(), "/path/to/file");

        let root = PathBuf::from("/");
        assert_eq!(root.as_str(), "/");

        let empty = PathBuf::new();
        assert_eq!(empty.as_str(), "");
    }

    #[def_test]
    fn test_path_normalization_complex() {
        let path = Pathname::new("/foo/bar/../baz/./qux/../file.txt");
        let normalized = path.normalize().unwrap();
        assert_eq!(normalized.as_str(), "/foo/baz/file.txt");

        let path = Pathname::new("/../../../test");
        assert!(path.normalize().is_none());

        let path = Pathname::new("././././test");
        let normalized = path.normalize().unwrap();
        assert_eq!(normalized.as_str(), "/test");
    }
}
