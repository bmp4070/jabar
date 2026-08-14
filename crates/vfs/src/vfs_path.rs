//! Paths to things the server can read, which are not all files on disk.
//!
//! A Rust analyzer only ever needs real filesystem paths, because every
//! dependency it sees is source. Java is not like that. Most of a Bazel
//! target's classpath arrives as jars, and the JDK's own types are not in a jar
//! at all — since Java 9 they live in `$JAVA_HOME/lib/modules`, a jimage
//! archive. Both hold things jabar must be able to name, hash, and hand back as
//! a location.
//!
//! So a [`VfsPath`] is one of three things, and the distinction is kept because
//! the three have genuinely different storage behind them:
//!
//! | Variant | Backed by | Example |
//! | --- | --- | --- |
//! | [`VfsPath::Real`] | the filesystem | `/repo/java/com/acme/core/Clock.java` |
//! | [`VfsPath::Jar`] | a zip archive | `jar:/repo/libpolicy.jar!/com/acme/policy/P.class` |
//! | [`VfsPath::Jrt`] | the JDK's jimage | `jrt:/java.base/java/lang/String.class` |
//!
//! The textual forms above round-trip through [`std::fmt::Display`] and
//! [`std::str::FromStr`], which is what lets a path survive the persisted index
//! and the LSP boundary. The `jar:` and `jrt:` spellings follow Java's own
//! conventions rather than being invented here — `jrt:` is specified by JEP 220.

use std::fmt;
use std::str::FromStr;

use paths::{AbsPathBuf, Utf8PathBuf};

/// Separates the archive from the entry within it, as in Java's `jar:` URLs.
const JAR_SEPARATOR: &str = "!/";
const JAR_SCHEME: &str = "jar:";
const JRT_SCHEME: &str = "jrt:/";

/// A path to something readable: a file on disk, or an entry inside an archive.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum VfsPath {
    /// A file on the real filesystem.
    Real(AbsPathBuf),
    /// An entry inside a jar. Both halves are needed: the same entry name
    /// appears in many jars, and the same jar is read many times.
    Jar(JarPath),
    /// An entry inside the JDK's module image.
    Jrt(JrtPath),
}

/// An entry inside a zip archive on disk.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct JarPath {
    jar: AbsPathBuf,
    /// Slash-separated, never leading-slashed, as zip entry names are stored.
    entry: String,
}

/// An entry inside the JDK's jimage archive, named by module.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct JrtPath {
    module: String,
    /// Slash-separated path within the module, e.g. `java/lang/String.class`.
    entry: String,
}

impl VfsPath {
    pub fn from_real(path: AbsPathBuf) -> VfsPath {
        VfsPath::Real(path)
    }

    /// Fails if `entry` is empty, since an archive with no entry names the
    /// archive itself and callers always mean a thing inside it.
    pub fn from_jar(jar: AbsPathBuf, entry: &str) -> Result<VfsPath, PathParseError> {
        Ok(VfsPath::Jar(JarPath::new(jar, entry)?))
    }

    /// Fails if either half is empty.
    pub fn from_jrt(module: &str, entry: &str) -> Result<VfsPath, PathParseError> {
        Ok(VfsPath::Jrt(JrtPath::new(module, entry)?))
    }

    /// The real filesystem path, for [`VfsPath::Real`] only.
    ///
    /// Archive entries deliberately return `None` rather than the containing
    /// archive's path: callers that want to open a file must not silently get
    /// a jar and read the wrong bytes.
    pub fn as_real(&self) -> Option<&AbsPathBuf> {
        match self {
            VfsPath::Real(path) => Some(path),
            VfsPath::Jar(_) | VfsPath::Jrt(_) => None,
        }
    }

    /// The archive that has to be opened to read this path, if any. This is the
    /// file to watch for invalidation — an entry has no mtime of its own.
    pub fn backing_file(&self) -> Option<&AbsPathBuf> {
        match self {
            VfsPath::Real(path) => Some(path),
            VfsPath::Jar(jar) => Some(&jar.jar),
            VfsPath::Jrt(_) => None,
        }
    }

    /// True when reading this needs no archive to be opened.
    pub fn is_real(&self) -> bool {
        matches!(self, VfsPath::Real(_))
    }

    /// Final path component: `Clock.java`, `String.class`.
    pub fn file_name(&self) -> Option<&str> {
        match self {
            VfsPath::Real(path) => path.file_name(),
            VfsPath::Jar(JarPath { entry, .. }) | VfsPath::Jrt(JrtPath { entry, .. }) => {
                entry.rsplit('/').next().filter(|it| !it.is_empty())
            }
        }
    }

    /// Extension without the leading dot: `java`, `class`.
    pub fn extension(&self) -> Option<&str> {
        let name = self.file_name()?;
        // `rsplit_once` rather than `split_once` so `Foo.Bar.class` yields
        // `class`, and the guard rejects dotfiles like `.gitignore`.
        let (stem, ext) = name.rsplit_once('.')?;
        (!stem.is_empty() && !ext.is_empty()).then_some(ext)
    }
}

impl JarPath {
    fn new(jar: AbsPathBuf, entry: &str) -> Result<JarPath, PathParseError> {
        let entry = normalize_entry(entry)?;
        Ok(JarPath { jar, entry })
    }

    pub fn jar(&self) -> &AbsPathBuf {
        &self.jar
    }

    pub fn entry(&self) -> &str {
        &self.entry
    }
}

impl JrtPath {
    fn new(module: &str, entry: &str) -> Result<JrtPath, PathParseError> {
        if module.is_empty() || module.contains('/') {
            return Err(PathParseError::BadModule(module.to_owned()));
        }
        let entry = normalize_entry(entry)?;
        Ok(JrtPath { module: module.to_owned(), entry })
    }

    pub fn module(&self) -> &str {
        &self.module
    }

    pub fn entry(&self) -> &str {
        &self.entry
    }
}

/// Strips a leading slash and rejects the empty entry.
///
/// Zip stores entry names without a leading slash, but `jar:` URLs write one
/// after the `!`. Normalizing here keeps the two spellings from producing two
/// different `FileId`s for the same bytes.
fn normalize_entry(entry: &str) -> Result<String, PathParseError> {
    let entry = entry.strip_prefix('/').unwrap_or(entry);
    if entry.is_empty() {
        return Err(PathParseError::EmptyEntry);
    }
    Ok(entry.to_owned())
}

/// Why a string could not be read as a [`VfsPath`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PathParseError {
    /// A `jar:` path with no `!/` separating archive from entry.
    MissingJarSeparator(String),
    /// The entry half was empty, which would name the archive itself.
    EmptyEntry,
    /// A module name that was empty or contained a slash.
    BadModule(String),
    /// A real or jar path that was not absolute.
    NotAbsolute(Utf8PathBuf),
    /// A `jrt:` path with no `/` after the module name.
    MissingJrtEntry(String),
}

impl fmt::Display for PathParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PathParseError::MissingJarSeparator(s) => {
                write!(f, "jar path is missing the `{JAR_SEPARATOR}` separator: `{s}`")
            }
            PathParseError::EmptyEntry => f.write_str("archive entry is empty"),
            PathParseError::BadModule(m) => write!(f, "not a module name: `{m}`"),
            PathParseError::NotAbsolute(p) => write!(f, "path is not absolute: `{p}`"),
            PathParseError::MissingJrtEntry(s) => {
                write!(f, "jrt path has a module but no entry: `{s}`")
            }
        }
    }
}

impl std::error::Error for PathParseError {}

impl fmt::Display for VfsPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            VfsPath::Real(path) => write!(f, "{path}"),
            VfsPath::Jar(JarPath { jar, entry }) => {
                write!(f, "{JAR_SCHEME}{jar}{JAR_SEPARATOR}{entry}")
            }
            VfsPath::Jrt(JrtPath { module, entry }) => {
                write!(f, "{JRT_SCHEME}{module}/{entry}")
            }
        }
    }
}

impl FromStr for VfsPath {
    type Err = PathParseError;

    fn from_str(s: &str) -> Result<VfsPath, PathParseError> {
        if let Some(rest) = s.strip_prefix(JAR_SCHEME) {
            let (jar, entry) = rest
                .split_once(JAR_SEPARATOR)
                .ok_or_else(|| PathParseError::MissingJarSeparator(s.to_owned()))?;
            return VfsPath::from_jar(parse_abs(jar)?, entry);
        }
        if let Some(rest) = s.strip_prefix(JRT_SCHEME) {
            let (module, entry) = rest
                .split_once('/')
                .ok_or_else(|| PathParseError::MissingJrtEntry(s.to_owned()))?;
            return VfsPath::from_jrt(module, entry);
        }
        Ok(VfsPath::Real(parse_abs(s)?))
    }
}

fn parse_abs(s: &str) -> Result<AbsPathBuf, PathParseError> {
    AbsPathBuf::try_from(s).map_err(PathParseError::NotAbsolute)
}

impl From<AbsPathBuf> for VfsPath {
    fn from(path: AbsPathBuf) -> VfsPath {
        VfsPath::Real(path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn abs(s: &str) -> AbsPathBuf {
        AbsPathBuf::try_from(s).expect("test path should be absolute")
    }

    fn parse(s: &str) -> VfsPath {
        s.parse().unwrap_or_else(|e| panic!("`{s}` should parse: {e}"))
    }

    #[test]
    fn real_paths_round_trip() {
        let p = parse("/repo/java/com/acme/core/Clock.java");
        assert!(p.is_real());
        assert_eq!(p.file_name(), Some("Clock.java"));
        assert_eq!(p.extension(), Some("java"));
        assert_eq!(p.to_string(), "/repo/java/com/acme/core/Clock.java");
        assert_eq!(p.as_real(), Some(&abs("/repo/java/com/acme/core/Clock.java")));
    }

    #[test]
    fn jar_paths_round_trip() {
        let s =
            "jar:/repo/bazel-bin/policy/libpolicy.jar!/com/acme/policy/DefaultRetryPolicy.class";
        let p = parse(s);
        assert_eq!(p.to_string(), s);
        assert_eq!(p.file_name(), Some("DefaultRetryPolicy.class"));
        assert_eq!(p.extension(), Some("class"));
        let VfsPath::Jar(jar) = &p else { panic!("expected a jar path, got {p:?}") };
        assert_eq!(jar.jar(), &abs("/repo/bazel-bin/policy/libpolicy.jar"));
        assert_eq!(jar.entry(), "com/acme/policy/DefaultRetryPolicy.class");
    }

    #[test]
    fn jrt_paths_round_trip() {
        let s = "jrt:/java.base/java/lang/String.class";
        let p = parse(s);
        assert_eq!(p.to_string(), s);
        assert_eq!(p.file_name(), Some("String.class"));
        let VfsPath::Jrt(jrt) = &p else { panic!("expected a jrt path, got {p:?}") };
        assert_eq!(jrt.module(), "java.base");
        assert_eq!(jrt.entry(), "java/lang/String.class");
    }

    #[test]
    fn jar_entry_slash_is_normalized_away() {
        // The two spellings must intern to one path, or the same classfile gets
        // two FileIds and the index answers the same query twice.
        let with = VfsPath::from_jar(abs("/x/a.jar"), "/com/A.class").unwrap();
        let without = VfsPath::from_jar(abs("/x/a.jar"), "com/A.class").unwrap();
        assert_eq!(with, without);
        assert_eq!(with.to_string(), "jar:/x/a.jar!/com/A.class");
    }

    #[test]
    fn archive_entries_are_not_real_paths() {
        // Handing back the jar here would make a caller read the archive's
        // bytes and believe they were the classfile's.
        let p = parse("jar:/x/a.jar!/com/A.class");
        assert_eq!(p.as_real(), None);
        assert!(!p.is_real());
        assert_eq!(p.backing_file(), Some(&abs("/x/a.jar")));
    }

    #[test]
    fn jrt_has_no_backing_file() {
        // The jimage lives inside the JDK, found via the toolchain rather than
        // by path, so there is nothing here for a file watcher to watch.
        assert_eq!(parse("jrt:/java.base/java/lang/String.class").backing_file(), None);
    }

    #[test]
    fn malformed_paths_are_rejected() {
        use PathParseError::*;
        assert!(matches!("jar:/x/a.jar".parse::<VfsPath>(), Err(MissingJarSeparator(_))));
        assert!(matches!("jar:/x/a.jar!/".parse::<VfsPath>(), Err(EmptyEntry)));
        assert!(matches!("jar:relative.jar!/A.class".parse::<VfsPath>(), Err(NotAbsolute(_))));
        assert!(matches!("jrt:/java.base".parse::<VfsPath>(), Err(MissingJrtEntry(_))));
        assert!(matches!("jrt://java/lang/String.class".parse::<VfsPath>(), Err(BadModule(_))));
        assert!(matches!("relative/path.java".parse::<VfsPath>(), Err(NotAbsolute(_))));
    }

    #[test]
    fn extension_edge_cases() {
        assert_eq!(parse("/x/Foo.Bar.class").extension(), Some("class"));
        assert_eq!(parse("/x/Makefile").extension(), None);
        assert_eq!(parse("/x/.gitignore").extension(), None, "a dotfile has no extension");
        assert_eq!(parse("/x/trailing.").extension(), None);
    }

    #[test]
    fn non_ascii_survives_the_round_trip() {
        // The fixture has a `grüße` identifier; filenames get the same treatment.
        let p = parse("/repo/java/com/acme/i18n/Grüße.java");
        assert_eq!(p.file_name(), Some("Grüße.java"));
        assert_eq!(p.to_string().parse::<VfsPath>().unwrap(), p);

        let j = parse("jar:/x/日本.jar!/com/acme/Grüße.class");
        assert_eq!(j.file_name(), Some("Grüße.class"));
        assert_eq!(j.to_string().parse::<VfsPath>().unwrap(), j);
    }

    #[test]
    fn variants_with_equal_text_are_distinct() {
        // Nothing should make a real path and an archive entry compare equal
        // just because their tails match.
        let real = parse("/com/A.class");
        let jarred = parse("jar:/x/a.jar!/com/A.class");
        assert_ne!(real, jarred);
    }
}
