//! Converting between LSP URIs and [`VfsPath`]s.
//!
//! Most of a Java classpath is not on the filesystem, so this boundary carries
//! more than `file:`. The three schemes match the [`VfsPath`] variants and use
//! Java's own spellings, which is what lets a location the server returns be
//! handed straight back to it later:
//!
//! ```text
//! file:///repo/java/com/acme/core/Clock.java
//! jar:/repo/libpolicy.jar!/com/acme/policy/DefaultRetryPolicy.class
//! jrt:/java.base/java/lang/String.class
//! ```

use lsp_types::Url;
use paths::AbsPathBuf;
use vfs::VfsPath;

/// Reads a client URI as a path the VFS understands.
pub fn vfs_path(uri: &Url) -> Result<VfsPath, UriError> {
    match uri.scheme() {
        "file" => {
            let path = uri.to_file_path().map_err(|()| UriError::NotAPath(uri.clone()))?;
            let path =
                AbsPathBuf::try_from_std(path).map_err(|_| UriError::NotAbsolute(uri.clone()))?;
            Ok(VfsPath::Real(path))
        }
        // `jar:` and `jrt:` round-trip through `VfsPath`'s own textual form,
        // which is where their grammar is defined.
        "jar" | "jrt" => uri.as_str().parse().map_err(|_| UriError::Malformed(uri.clone())),
        _ => Err(UriError::UnsupportedScheme(uri.clone())),
    }
}

/// Renders a path as a URI to hand back to the client.
pub fn to_uri(path: &VfsPath) -> Result<Url, UriError> {
    match path {
        VfsPath::Real(abs) => Url::from_file_path(abs.as_str())
            .map_err(|()| UriError::NotAPath(Url::parse("file:///").expect("valid"))),
        // Already in their canonical textual form.
        VfsPath::Jar(_) | VfsPath::Jrt(_) => {
            Url::parse(&path.to_string()).map_err(|_| UriError::Unrenderable(path.to_string()))
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UriError {
    /// A `file:` URI that does not name a path — a UNC host, or `file://x/y`.
    NotAPath(Url),
    NotAbsolute(Url),
    /// A `jar:` or `jrt:` URI that does not parse as a [`VfsPath`].
    Malformed(Url),
    /// A scheme the server does not handle, such as `untitled:` for a buffer
    /// that has never been saved.
    UnsupportedScheme(Url),
    Unrenderable(String),
}

impl std::fmt::Display for UriError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            UriError::NotAPath(uri) => write!(f, "URI does not name a file path: `{uri}`"),
            UriError::NotAbsolute(uri) => write!(f, "URI is not an absolute path: `{uri}`"),
            UriError::Malformed(uri) => write!(f, "URI is not a valid archive path: `{uri}`"),
            UriError::UnsupportedScheme(uri) => {
                write!(f, "unsupported URI scheme `{}`: `{uri}`", uri.scheme())
            }
            UriError::Unrenderable(path) => write!(f, "path cannot be rendered as a URI: `{path}`"),
        }
    }
}

impl std::error::Error for UriError {}

#[cfg(test)]
mod tests {
    use super::*;

    fn url(s: &str) -> Url {
        Url::parse(s).unwrap_or_else(|e| panic!("`{s}` should parse: {e}"))
    }

    #[test]
    fn file_uris_round_trip() {
        let uri = url("file:///repo/java/com/acme/core/Clock.java");
        let path = vfs_path(&uri).expect("should convert");
        assert_eq!(path.to_string(), "/repo/java/com/acme/core/Clock.java");
        assert_eq!(to_uri(&path).unwrap(), uri);
    }

    #[test]
    fn percent_encoding_is_decoded() {
        // Clients percent-encode spaces and non-ASCII. Failing to decode would
        // make the VFS intern a path that does not exist.
        let uri = url("file:///repo/My%20Project/Gr%C3%BC%C3%9Fe.java");
        let path = vfs_path(&uri).expect("should convert");
        assert_eq!(path.to_string(), "/repo/My Project/Grüße.java");
    }

    #[test]
    fn non_ascii_survives_the_round_trip() {
        let path: VfsPath = "/repo/java/com/acme/i18n/Grüße.java".parse().unwrap();
        let uri = to_uri(&path).expect("should render");
        assert_eq!(vfs_path(&uri).unwrap(), path, "re-reading our own URI must give the same path");
    }

    #[test]
    fn jar_uris_round_trip() {
        let uri = url("jar:/repo/libpolicy.jar!/com/acme/policy/DefaultRetryPolicy.class");
        let path = vfs_path(&uri).expect("should convert");
        assert!(matches!(path, VfsPath::Jar(_)));
        assert_eq!(path.file_name(), Some("DefaultRetryPolicy.class"));
        assert_eq!(to_uri(&path).unwrap().as_str(), uri.as_str());
    }

    #[test]
    fn jrt_uris_round_trip() {
        let uri = url("jrt:/java.base/java/lang/String.class");
        let path = vfs_path(&uri).expect("should convert");
        assert!(matches!(path, VfsPath::Jrt(_)));
        assert_eq!(to_uri(&path).unwrap().as_str(), uri.as_str());
    }

    #[test]
    fn unsupported_schemes_are_refused_by_name() {
        // `untitled:` is what editors use for a buffer never written to disk.
        // Refusing it explicitly beats treating it as a path that happens not to
        // exist, which would look like a missing file.
        let err = vfs_path(&url("untitled:Untitled-1")).expect_err("should refuse");
        assert!(matches!(err, UriError::UnsupportedScheme(_)));
        assert!(err.to_string().contains("untitled"));

        assert!(matches!(
            vfs_path(&url("https://example.com/A.java")),
            Err(UriError::UnsupportedScheme(_))
        ));
    }

    #[test]
    fn malformed_archive_uris_are_refused() {
        // A `jar:` URI with no `!/` names an archive, not an entry in one.
        assert!(matches!(vfs_path(&url("jar:/repo/a.jar")), Err(UriError::Malformed(_))));
        assert!(matches!(vfs_path(&url("jrt:/java.base")), Err(UriError::Malformed(_))));
    }
}
