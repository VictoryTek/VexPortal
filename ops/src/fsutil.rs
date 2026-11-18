//! Writing files the way the daemon needs to: whole or not at all, and never readable
//! by anyone who should not read them.

use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

/// Replace `path` with `bytes` in one step.
///
/// The data goes to a temporary file in the same directory, created with `mode` from
/// the start so a secret is never briefly world-readable, then renamed over `path`.
/// A crash leaves either the old file or the new one, never half of each.
pub fn write_atomic(path: &Path, bytes: &[u8], mode: u32) -> io::Result<()> {
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    let name = path
        .file_name()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "path has no file name"))?
        .to_string_lossy();
    // The counter keeps two jobs writing the same file from colliding on one temp name;
    // the process id keeps a leftover from a crashed run from blocking a later one.
    static NEXT: AtomicU32 = AtomicU32::new(0);
    let temp = dir.join(format!(
        ".{name}.vexportal-{}-{}.tmp",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));

    // `create_new` refuses an existing file or a symlink planted at the temp name, so a
    // writer in a shared directory cannot redirect this write.
    write_via(&temp, path, bytes, mode)
}

/// [`write_atomic`] with the temp file's path chosen by the caller.
fn write_via(temp: &Path, path: &Path, bytes: &[u8], mode: u32) -> io::Result<()> {
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(mode)
        .open(temp)?;

    // From here the temp file is ours, so any failure removes it.
    let result = (|| {
        // The creation mode is cut down by the umask; this sets exactly what was asked
        // for, so an edited file keeps its original permissions.
        file.set_permissions(fs::Permissions::from_mode(mode))?;
        file.write_all(bytes)?;
        file.sync_all()?;
        fs::rename(temp, path)
    })();

    if result.is_err() {
        let _ = fs::remove_file(temp);
    }
    result
}

/// Create `path` with `bytes` and `mode`, failing if it already exists.
///
/// For things that must never be replaced by accident, such as a freshly generated
/// key: [`write_atomic`] would overwrite whatever was there.
pub fn create_new(path: &Path, bytes: &[u8], mode: u32) -> io::Result<()> {
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(mode)
        .open(path)?;
    let result = (|| {
        file.set_permissions(fs::Permissions::from_mode(mode))?;
        file.write_all(bytes)?;
        file.sync_all()
    })();
    if result.is_err() {
        // Half a key is worse than none.
        let _ = fs::remove_file(path);
    }
    result
}

/// `bytes` random bytes from the kernel, as lowercase hex. For generated secrets.
pub fn random_hex(bytes: usize) -> io::Result<String> {
    use std::fmt::Write as _;
    use std::io::Read;
    let mut buf = vec![0u8; bytes];
    fs::File::open("/dev/urandom")?.read_exact(&mut buf)?;
    let mut out = String::with_capacity(bytes * 2);
    for b in buf {
        let _ = write!(out, "{b:02x}");
    }
    Ok(out)
}

/// The permission bits of an existing file, so an edit keeps them.
pub fn mode_of(path: &Path) -> io::Result<u32> {
    Ok(fs::metadata(path)?.mode() & 0o7777)
}

/// Copy `path` to `path.bak`, with the same permissions, replacing an older backup.
pub fn backup(path: &Path) -> io::Result<PathBuf> {
    let mut name = path
        .file_name()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "path has no file name"))?
        .to_os_string();
    name.push(".bak");
    let target = path.with_file_name(name);
    write_atomic(&target, &fs::read(path)?, mode_of(path)?)?;
    Ok(target)
}

/// A private working directory that deletes itself. For a command that writes output
/// into its working directory, such as the `result` link of `nixos-rebuild build`.
pub struct Scratch(PathBuf);

impl Scratch {
    pub fn new() -> io::Result<Self> {
        use std::os::unix::fs::DirBuilderExt;
        static NEXT: AtomicU32 = AtomicU32::new(0);
        let path = std::env::temp_dir().join(format!(
            "vexportal-scratch-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        // `create` refuses an existing path, so a directory or link planted at this name
        // is an error rather than something this writes through.
        fs::DirBuilder::new().mode(0o700).create(&path)?;
        Ok(Self(path))
    }

    pub fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

pub mod testing {
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU32, Ordering};

    /// A scratch directory that deletes itself, for tests that touch real files.
    pub struct TempDir(PathBuf);

    impl TempDir {
        pub fn new() -> Self {
            static NEXT: AtomicU32 = AtomicU32::new(0);
            let path = std::env::temp_dir().join(format!(
                "vexportal-test-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir_all(&path).expect("create the test directory");
            Self(path)
        }

        pub fn path(&self) -> &Path {
            &self.0
        }

        pub fn join(&self, name: &str) -> PathBuf {
            self.0.join(name)
        }

        pub fn write(&self, name: &str, text: &str) -> PathBuf {
            let path = self.join(name);
            std::fs::write(&path, text).expect("write the test file");
            path
        }
    }

    impl Default for TempDir {
        fn default() -> Self {
            Self::new()
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::testing::TempDir;
    use super::*;

    #[test]
    fn writes_with_the_requested_mode_and_leaves_no_temp_file() {
        let dir = TempDir::new();
        let path = dir.join("secret");
        write_atomic(&path, b"token", 0o600).unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"token");
        assert_eq!(mode_of(&path).unwrap(), 0o600);
        let leftovers: Vec<_> = fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().ends_with(".tmp"))
            .collect();
        assert!(leftovers.is_empty());
    }

    #[test]
    fn replaces_an_existing_file_whole() {
        let dir = TempDir::new();
        let path = dir.write("f", "old contents that are longer");
        write_atomic(&path, b"new", 0o644).unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "new");
    }

    #[test]
    fn never_writes_through_a_link_planted_at_the_temp_name() {
        let dir = TempDir::new();
        let path = dir.join("f");
        let victim = dir.write("victim", "untouched");
        let temp = dir.join(".f.planted.tmp");
        std::os::unix::fs::symlink(&victim, &temp).unwrap();

        let err = write_via(&temp, &path, b"x", 0o644).unwrap_err();

        assert_eq!(err.kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(fs::read_to_string(&victim).unwrap(), "untouched");
        // The planted link is not ours to remove.
        assert!(fs::symlink_metadata(&temp)
            .unwrap()
            .file_type()
            .is_symlink());
        assert!(!path.exists());
    }

    #[test]
    fn two_writes_to_the_same_file_do_not_collide() {
        let dir = TempDir::new();
        let path = dir.join("f");
        write_atomic(&path, b"one", 0o644).unwrap();
        write_atomic(&path, b"two", 0o644).unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "two");
    }

    #[test]
    fn create_new_refuses_to_replace_a_file() {
        let dir = TempDir::new();
        let path = dir.join("key");
        create_new(&path, b"first", 0o600).unwrap();
        assert_eq!(mode_of(&path).unwrap(), 0o600);
        let err = create_new(&path, b"second", 0o600).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(fs::read(&path).unwrap(), b"first");
    }

    #[test]
    fn backup_copies_content_and_mode() {
        let dir = TempDir::new();
        let path = dir.join("features.nix");
        write_atomic(&path, b"{ }", 0o640).unwrap();
        let bak = backup(&path).unwrap();
        assert_eq!(bak.file_name().unwrap(), "features.nix.bak");
        assert_eq!(fs::read(&bak).unwrap(), b"{ }");
        assert_eq!(mode_of(&bak).unwrap(), 0o640);
    }
}
