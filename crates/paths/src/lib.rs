//! Absolute, UTF-8 filesystem paths.
//!
//! Two invariants are enforced by construction, and everything downstream leans
//! on both:
//!
//! * **Absolute.** A relative path in a language server is always a bug waiting
//!   to happen, because the process' working directory has nothing to do with
//!   the workspace root.
//! * **UTF-8.** LSP speaks `file://` URIs and Bazel speaks label strings, both
//!   of which are text. Carrying an [`std::path::PathBuf`] means every boundary
//!   needs a fallible conversion; carrying a [`Utf8PathBuf`] means the
//!   conversion happens once, here.
//!
//! Paths are *not* normalized beyond what the caller supplies. In particular a
//! path containing `..` is still absolute and still accepted — resolving those
//! requires touching the filesystem, which this crate deliberately never does.

use std::borrow::Borrow;
use std::fmt;
use std::ops::Deref;

pub use camino::{Utf8Path, Utf8PathBuf};

/// An owned absolute UTF-8 path.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct AbsPathBuf(Utf8PathBuf);

impl AbsPathBuf {
    /// Wraps `path`, returning it unchanged in the `Err` case if it is relative.
    pub fn try_new(path: Utf8PathBuf) -> Result<AbsPathBuf, Utf8PathBuf> {
        if path.is_absolute() { Ok(AbsPathBuf(path)) } else { Err(path) }
    }

    /// Wraps a `std` path, failing if it is relative or not valid UTF-8.
    pub fn try_from_std(path: std::path::PathBuf) -> Result<AbsPathBuf, std::path::PathBuf> {
        let utf8 = Utf8PathBuf::from_path_buf(path)?;
        AbsPathBuf::try_new(utf8).map_err(|it| it.into_std_path_buf())
    }

    pub fn as_path(&self) -> &AbsPath {
        AbsPath::new_unchecked(&self.0)
    }

    pub fn into_utf8_path_buf(self) -> Utf8PathBuf {
        self.0
    }

    /// Appends `path`. Panics if `path` is absolute, since that would silently
    /// discard `self` — [`Utf8PathBuf::push`] is defined to replace, and every
    /// caller here means "descend".
    pub fn push(&mut self, path: impl AsRef<Utf8Path>) {
        let path = path.as_ref();
        assert!(path.is_relative(), "cannot push absolute path `{path}` onto `{}`", self.0);
        self.0.push(path);
    }

    /// Removes the last component, returning whether there was one to remove.
    /// The root is left intact.
    pub fn pop(&mut self) -> bool {
        self.0.pop()
    }
}

impl Deref for AbsPathBuf {
    type Target = AbsPath;
    fn deref(&self) -> &AbsPath {
        self.as_path()
    }
}

impl AsRef<AbsPath> for AbsPathBuf {
    fn as_ref(&self) -> &AbsPath {
        self.as_path()
    }
}

impl AsRef<Utf8Path> for AbsPathBuf {
    fn as_ref(&self) -> &Utf8Path {
        &self.0
    }
}

impl Borrow<AbsPath> for AbsPathBuf {
    fn borrow(&self) -> &AbsPath {
        self.as_path()
    }
}

impl From<AbsPathBuf> for Utf8PathBuf {
    fn from(path: AbsPathBuf) -> Utf8PathBuf {
        path.0
    }
}

impl TryFrom<Utf8PathBuf> for AbsPathBuf {
    type Error = Utf8PathBuf;
    fn try_from(path: Utf8PathBuf) -> Result<AbsPathBuf, Utf8PathBuf> {
        AbsPathBuf::try_new(path)
    }
}

impl TryFrom<&str> for AbsPathBuf {
    type Error = Utf8PathBuf;
    fn try_from(path: &str) -> Result<AbsPathBuf, Utf8PathBuf> {
        AbsPathBuf::try_new(Utf8PathBuf::from(path))
    }
}

impl fmt::Display for AbsPathBuf {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&self.0, f)
    }
}

/// A borrowed absolute UTF-8 path. The unsized counterpart of [`AbsPathBuf`].
#[derive(Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(transparent)]
pub struct AbsPath(Utf8Path);

impl AbsPath {
    /// # Panics
    ///
    /// Panics if `path` is relative. Prefer [`AbsPath::try_new`] at boundaries;
    /// this exists for the case where absoluteness is already established.
    pub fn new_unchecked(path: &Utf8Path) -> &AbsPath {
        assert!(path.is_absolute(), "not an absolute path: `{path}`");
        // SAFETY: `AbsPath` is `#[repr(transparent)]` over `Utf8Path`.
        unsafe { &*(path as *const Utf8Path as *const AbsPath) }
    }

    pub fn try_new(path: &Utf8Path) -> Option<&AbsPath> {
        path.is_absolute().then(|| AbsPath::new_unchecked(path))
    }

    pub fn as_utf8_path(&self) -> &Utf8Path {
        &self.0
    }

    pub fn as_str(&self) -> &str {
        self.0.as_str()
    }

    pub fn to_path_buf(&self) -> AbsPathBuf {
        AbsPathBuf(self.0.to_path_buf())
    }

    /// The parent directory, or `None` at the filesystem root.
    pub fn parent(&self) -> Option<&AbsPath> {
        self.0.parent().map(AbsPath::new_unchecked)
    }

    /// Appends a relative path.
    ///
    /// # Panics
    ///
    /// Panics if `path` is absolute, for the reason given on
    /// [`AbsPathBuf::push`].
    pub fn join(&self, path: impl AsRef<Utf8Path>) -> AbsPathBuf {
        let mut buf = self.to_path_buf();
        buf.push(path);
        buf
    }

    /// Final component, if any. `None` for the root and for paths ending in `..`.
    pub fn file_name(&self) -> Option<&str> {
        self.0.file_name()
    }

    /// Extension without the leading dot.
    pub fn extension(&self) -> Option<&str> {
        self.0.extension()
    }

    pub fn starts_with(&self, base: &AbsPath) -> bool {
        self.0.starts_with(&base.0)
    }

    pub fn strip_prefix(&self, base: &AbsPath) -> Option<&Utf8Path> {
        self.0.strip_prefix(&base.0).ok()
    }
}

impl ToOwned for AbsPath {
    type Owned = AbsPathBuf;
    fn to_owned(&self) -> AbsPathBuf {
        self.to_path_buf()
    }
}

impl AsRef<Utf8Path> for AbsPath {
    fn as_ref(&self) -> &Utf8Path {
        &self.0
    }
}

impl fmt::Display for AbsPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&self.0, f)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn abs(s: &str) -> AbsPathBuf {
        AbsPathBuf::try_from(s).expect("test path should be absolute")
    }

    #[test]
    fn relative_paths_are_rejected() {
        assert!(AbsPathBuf::try_from("java/com/acme").is_err());
        assert!(AbsPathBuf::try_from("./x").is_err());
        assert!(AbsPathBuf::try_from("").is_err());
        assert!(AbsPath::try_new(Utf8Path::new("x")).is_none());
    }

    #[test]
    fn absolute_paths_round_trip() {
        let p = abs("/repo/java/com/acme/core/Clock.java");
        assert_eq!(p.file_name(), Some("Clock.java"));
        assert_eq!(p.extension(), Some("java"));
        assert_eq!(p.as_str(), "/repo/java/com/acme/core/Clock.java");
        assert_eq!(p.as_path().to_path_buf(), p);
    }

    #[test]
    fn join_descends() {
        let root = abs("/repo");
        assert_eq!(root.join("java/com").as_str(), "/repo/java/com");
    }

    #[test]
    #[should_panic(expected = "cannot push absolute path")]
    fn join_rejects_absolute() {
        abs("/repo").join("/etc/passwd");
    }

    #[test]
    fn parent_terminates_at_root() {
        let mut p = abs("/a/b");
        assert_eq!(p.parent().map(|it| it.as_str()), Some("/a"));
        assert!(p.pop());
        assert!(p.pop());
        assert_eq!(p.as_str(), "/");
        assert!(!p.pop(), "root has no parent to pop");
        assert!(abs("/").parent().is_none());
    }

    #[test]
    fn prefix_operations() {
        let root = abs("/repo");
        let file = abs("/repo/java/Main.java");
        assert!(file.starts_with(&root));
        assert!(!root.starts_with(&file));
        assert_eq!(file.strip_prefix(&root).map(|it| it.as_str()), Some("java/Main.java"));
        assert_eq!(file.strip_prefix(&abs("/other")), None);
    }

    #[test]
    fn non_ascii_paths_survive() {
        let p = abs("/repo/java/com/acme/i18n/Grüße.java");
        assert_eq!(p.file_name(), Some("Grüße.java"));
        assert_eq!(p.as_str().len(), "/repo/java/com/acme/i18n/Grüße.java".len());
    }
}
