//! Workspace-relative file operations for paths supplied by tools.
use cap_fs_ext::{DirExt, FollowSymlinks, OpenOptionsFollowExt, OpenOptionsSyncExt};
use cap_std::{
    ambient_authority,
    fs::{Dir, File, OpenOptions},
};
use std::io::{self, Read, Write};
use std::path::{Component, Path, PathBuf};

#[derive(Debug)]
pub struct WorkspaceFs {
    root: PathBuf,
    original: PathBuf,
    dir: Dir,
}

fn invalid_path() -> io::Error {
    io::Error::new(
        io::ErrorKind::PermissionDenied,
        "path is outside the workspace or cannot be resolved safely",
    )
}

impl WorkspaceFs {
    pub fn open(root: &Path) -> io::Result<Self> {
        let original = std::path::absolute(root)?;
        let root = original.canonicalize()?;
        let dir = Dir::open_ambient_dir(&root, ambient_authority())?;
        Ok(Self {
            root,
            original,
            dir,
        })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Reject rooted drive-relative paths, device names, ADS and path traversal.
    pub fn relative(&self, path: &Path) -> io::Result<PathBuf> {
        let path = if path.is_absolute() {
            path.strip_prefix(&self.root)
                .or_else(|_| path.strip_prefix(&self.original))
                .map_err(|_| invalid_path())?
        } else {
            path
        };
        let mut relative = PathBuf::new();
        for component in path.components() {
            match component {
                Component::CurDir => {}
                Component::Normal(name) if safe_component(name) => relative.push(name),
                _ => return Err(invalid_path()),
            }
        }
        Ok(relative)
    }

    /// Resolve links, retaining a missing suffix for operations that can create it.
    pub fn resolve(&self, path: &Path, allow_missing: bool) -> io::Result<PathBuf> {
        self.resolve_parts(PathBuf::new(), &self.relative(path)?, allow_missing, &mut 0)
    }

    fn resolve_parts(
        &self,
        mut resolved: PathBuf,
        path: &Path,
        allow_missing: bool,
        links: &mut usize,
    ) -> io::Result<PathBuf> {
        let mut parts = path.components();
        while let Some(part) = parts.next() {
            let name = match part {
                Component::CurDir => continue,
                Component::ParentDir if resolved.pop() => continue,
                Component::Normal(name) if safe_component(name) => name,
                _ => return Err(invalid_path()),
            };
            let parent = self.open_directory(&resolved)?;
            match parent.symlink_metadata(name) {
                Ok(meta) if is_link(&meta) => {
                    *links += 1;
                    if *links > 40 {
                        return Err(invalid_path());
                    }
                    let target = parent.read_link_contents(name)?;
                    resolved = if target.is_absolute() {
                        self.resolve_parts(PathBuf::new(), &self.relative(&target)?, false, links)?
                    } else {
                        self.resolve_parts(resolved, &target, false, links)?
                    };
                }
                Ok(meta) => {
                    if !meta.is_file() && !meta.is_dir() {
                        return Err(invalid_path());
                    }
                    resolved.push(name);
                }
                Err(err) if allow_missing && err.kind() == io::ErrorKind::NotFound => {
                    resolved.push(name);
                    resolved.push(self.relative(parts.as_path())?);
                    return Ok(resolved);
                }
                Err(err) => return Err(err),
            }
        }
        Ok(resolved)
    }

    fn open_directory(&self, path: &Path) -> io::Result<Dir> {
        let mut dir = self.dir.try_clone()?;
        for component in self.relative(path)?.components() {
            dir = dir.open_dir_nofollow(component.as_os_str())?;
        }
        Ok(dir)
    }

    pub fn read_dir(&self, resolved: &Path) -> io::Result<cap_std::fs::ReadDir> {
        self.open_directory(resolved)?.entries()
    }

    pub fn open_file(&self, resolved: &Path) -> io::Result<File> {
        self.file(resolved, false, false)
    }

    pub fn read(&self, resolved: &Path) -> io::Result<Vec<u8>> {
        let mut bytes = Vec::new();
        self.open_file(resolved)?.read_to_end(&mut bytes)?;
        Ok(bytes)
    }

    pub fn read_to_string(&self, resolved: &Path) -> io::Result<String> {
        let mut text = String::new();
        self.open_file(resolved)?.read_to_string(&mut text)?;
        Ok(text)
    }

    pub fn write(&self, resolved: &Path, content: &[u8], create: bool) -> io::Result<()> {
        let mut file = self.file(resolved, true, create)?;
        file.set_len(0)?;
        file.write_all(content)
    }

    pub fn copy_from(&self, resolved: &Path, input: &mut impl Read) -> io::Result<u64> {
        let mut output = self.file(resolved, true, true)?;
        output.set_len(0)?;
        io::copy(input, &mut output)
    }

    fn file(&self, path: &Path, write: bool, create: bool) -> io::Result<File> {
        let path = self.relative(path)?;
        let name = path.file_name().ok_or_else(invalid_path)?;
        let mut dir = self.dir.try_clone()?;
        for component in path.parent().unwrap_or(Path::new("")).components() {
            let part = component.as_os_str();
            dir = match dir.open_dir_nofollow(part) {
                Err(err) if create && err.kind() == io::ErrorKind::NotFound => {
                    match dir.create_dir(part) {
                        Err(err) if err.kind() != io::ErrorKind::AlreadyExists => return Err(err),
                        _ => {}
                    }
                    dir.open_dir_nofollow(part)?
                }
                other => other?,
            };
        }
        match dir.symlink_metadata(name) {
            Ok(meta) if is_link(&meta) || !meta.is_file() => return Err(invalid_path()),
            Err(err) if !create || err.kind() != io::ErrorKind::NotFound => return Err(err),
            _ => {}
        }
        let mut options = OpenOptions::new();
        options
            .read(!write)
            .write(write)
            .create(create)
            .follow(FollowSymlinks::No)
            .nonblock(true);
        let file = dir.open_with(name, &options)?;
        if !file.metadata()?.is_file() {
            return Err(invalid_path());
        }
        Ok(file)
    }
}

#[cfg(not(windows))]
fn safe_component(_: &std::ffi::OsStr) -> bool {
    true
}

#[cfg(windows)]
fn safe_component(name: &std::ffi::OsStr) -> bool {
    let name = name.to_string_lossy();
    let stem = name
        .split('.')
        .next()
        .unwrap_or_default()
        .trim_end()
        .to_ascii_uppercase();
    !name.ends_with([' ', '.'])
        && !name.contains([':', '<', '>', '"', '|', '?', '*'])
        && !matches!(
            stem.as_str(),
            "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$"
        )
        && !stem
            .strip_prefix("COM")
            .or_else(|| stem.strip_prefix("LPT"))
            .is_some_and(|suffix| {
                matches!(
                    suffix,
                    "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9" | "¹" | "²" | "³"
                )
            })
}

#[cfg(not(windows))]
fn is_link(meta: &cap_std::fs::Metadata) -> bool {
    meta.file_type().is_symlink()
}

#[cfg(windows)]
fn is_link(meta: &cap_std::fs::Metadata) -> bool {
    use cap_std::fs::MetadataExt;
    const REPARSE_POINT: u32 = 0x400;
    meta.file_attributes() & REPARSE_POINT != 0
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn workspace_resolved_target_cannot_be_replaced_with_link() {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join("dir")).unwrap();
        std::fs::write(root.path().join("dir/file"), "inside").unwrap();
        std::fs::write(outside.path().join("file"), "outside").unwrap();
        let workspace = WorkspaceFs::open(root.path()).unwrap();
        let path = workspace.resolve(Path::new("dir/file"), false).unwrap();
        std::fs::rename(root.path().join("dir"), root.path().join("previous")).unwrap();
        std::os::unix::fs::symlink(outside.path(), root.path().join("dir")).unwrap();
        assert!(workspace.read(&path).is_err());
        assert!(workspace.write(&path, b"changed", true).is_err());
        assert_eq!(
            std::fs::read(outside.path().join("file")).unwrap(),
            b"outside"
        );
    }
}
