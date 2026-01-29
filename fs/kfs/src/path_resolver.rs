//! Path resolution module
//!
//! Provides stateless path resolution functionality, separated from filesystem operations.

use alloc::{borrow::ToOwned, string::String};

use fs_ng_vfs::{
    Location, NodeType, VfsError, VfsResult,
    path::{Component, Components, Path, PathBuf},
};

/// Default maximum symlink follow depth
pub const DEFAULT_MAX_SYMLINKS: usize = 40;

/// Path resolver - stateless path resolution logic
///
/// This component handles all path resolution logic including:
/// - Absolute and relative path resolution
/// - Symlink following with loop detection
/// - Path component normalization (`.` and `..`)
#[derive(Debug, Clone)]
pub struct PathResolver {
    max_symlinks: usize,
}

impl PathResolver {
    /// Creates a new path resolver with default settings
    #[inline]
    pub fn new() -> Self {
        Self {
            max_symlinks: DEFAULT_MAX_SYMLINKS,
        }
    }

    /// Creates a path resolver with custom max symlink depth
    #[inline]
    pub fn with_max_symlinks(max: usize) -> Self {
        Self { max_symlinks: max }
    }

    /// Resolves a path starting from the given base location
    ///
    /// # Arguments
    /// * `base` - The base directory to resolve from (typically root or cwd)
    /// * `path` - The path to resolve
    /// * `follow_symlinks` - Whether to follow symlinks
    ///
    /// # Returns
    /// The resolved `Location`, or an error if the path doesn't exist or a loop is detected
    pub fn resolve(
        &self,
        base: &Location,
        path: &Path,
        follow_symlinks: bool,
    ) -> VfsResult<Location> {
        let mut follow_count = 0;
        self.resolve_with_count(base, path, follow_symlinks, &mut follow_count)
    }

    /// Internal resolution with symlink counter
    fn resolve_with_count(
        &self,
        base: &Location,
        path: &Path,
        follow_symlinks: bool,
        follow_count: &mut usize,
    ) -> VfsResult<Location> {
        let entry_name = path.file_name();
        let mut components = path.components();

        // If path has a file name, we need to resolve parent first
        if entry_name.is_some() {
            components.next_back();
        }

        let dir = self.resolve_components(base, components, follow_count)?;
        dir.check_is_dir()?;

        match entry_name {
            Some(name) => {
                if follow_symlinks {
                    self.lookup(&dir, name, follow_count)
                } else {
                    dir.lookup_no_follow(name)
                }
            }
            None => Ok(dir),
        }
    }

    /// Resolves a path to its parent directory and entry name
    ///
    /// # Returns
    /// `(parent_directory, entry_name)` tuple
    pub fn resolve_parent(&self, base: &Location, path: &Path) -> VfsResult<(Location, String)> {
        let (dir, name) = self.resolve_inner(base, path, &mut 0)?;
        if let Some(name) = name {
            Ok((dir, name.to_owned()))
        } else if let Some(parent) = dir.parent() {
            Ok((parent, dir.name().to_owned()))
        } else {
            Err(VfsError::InvalidInput)
        }
    }

    /// Resolves a path that is expected not to exist (for creation)
    ///
    /// Verifies that the parent directory exists but the entry doesn't
    pub fn resolve_nonexistent<'a>(
        &self,
        base: &Location,
        path: &'a Path,
    ) -> VfsResult<(Location, &'a str)> {
        let (dir, name) = self.resolve_inner(base, path, &mut 0)?;
        if let Some(name) = name {
            Ok((dir, name))
        } else {
            Err(VfsError::InvalidInput)
        }
    }

    /// Internal helper for resolve_parent and resolve_nonexistent
    fn resolve_inner<'a>(
        &self,
        base: &Location,
        path: &'a Path,
        follow_count: &mut usize,
    ) -> VfsResult<(Location, Option<&'a str>)> {
        let entry_name = path.file_name();
        let mut components = path.components();
        if entry_name.is_some() {
            components.next_back();
        }
        let dir = self.resolve_components(base, components, follow_count)?;
        dir.check_is_dir()?;
        Ok((dir, entry_name))
    }

    /// Resolves path components iteratively (public for internal use)
    #[doc(hidden)]
    pub fn resolve_components_internal(
        &self,
        base: &Location,
        components: Components,
        follow_count: &mut usize,
    ) -> VfsResult<Location> {
        self.resolve_components(base, components, follow_count)
    }

    /// Resolves path components iteratively
    fn resolve_components(
        &self,
        base: &Location,
        components: Components,
        follow_count: &mut usize,
    ) -> VfsResult<Location> {
        let mut current = base.clone();

        for comp in components {
            match comp {
                Component::CurDir => {
                    // `.` - stay in current directory
                }
                Component::ParentDir => {
                    // `..` - go to parent
                    current = current.parent().unwrap_or_else(|| base.clone());
                }
                Component::RootDir => {
                    // `/` - go to root
                    current = self.find_root(&current);
                }
                Component::Normal(name) => {
                    // Regular component - lookup and potentially follow symlink
                    current = self.lookup(&current, name, follow_count)?;
                }
            }
        }

        Ok(current)
    }

    /// Looks up a name in a directory and follows symlinks if needed
    fn lookup(&self, dir: &Location, name: &str, follow_count: &mut usize) -> VfsResult<Location> {
        let loc = dir.lookup_no_follow(name)?;
        self.try_resolve_symlink(dir, loc, follow_count)
    }

    /// Attempts to resolve a symlink
    fn try_resolve_symlink(
        &self,
        base: &Location,
        loc: Location,
        follow_count: &mut usize,
    ) -> VfsResult<Location> {
        if loc.node_type() != NodeType::Symlink {
            return Ok(loc);
        }

        if *follow_count >= self.max_symlinks {
            return Err(VfsError::FilesystemLoop);
        }

        *follow_count += 1;
        let target = loc.read_link()?;
        if target.is_empty() {
            return Err(VfsError::NotFound);
        }

        // Resolve the symlink target
        self.resolve_components(base, PathBuf::from(target).components(), follow_count)
    }

    /// Finds the root of the filesystem
    fn find_root(&self, loc: &Location) -> Location {
        let mut current = loc.clone();
        while let Some(parent) = current.parent() {
            current = parent;
        }
        current
    }
}

impl Default for PathResolver {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_path_resolver_new() {
        let resolver = PathResolver::new();
        assert_eq!(resolver.max_symlinks, DEFAULT_MAX_SYMLINKS);
    }

    #[test]
    fn test_path_resolver_with_max_symlinks() {
        let resolver = PathResolver::with_max_symlinks(10);
        assert_eq!(resolver.max_symlinks, 10);
    }
}
