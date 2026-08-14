//! Bazel target labels.
//!
//! Labels are the identity of everything in this system: a file belongs to a
//! target, a target owns a classpath, an index shard is keyed by target. Parsing
//! them once into a real type — rather than passing `String`s around and
//! `split(':')`-ing at each use — is what keeps that identity from quietly
//! becoming "some text that came out of bazel".
//!
//! The grammar accepted here is the one Bazel's own output uses:
//!
//! ```text
//! //java/com/acme/policy:policy      canonical
//! //java/com/acme/policy             abbreviated; name defaults to `policy`
//! @@//java/com/acme/app:app          main repo, bzlmod canonical spelling
//! @rules_java//java:defs             an external repo
//! @@rules_java+//toolchains:current  external repo, canonical spelling
//! ```

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

/// A parsed Bazel label.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct TargetLabel {
    /// Repository, without the `@` or `@@` sigil. Empty for the main repo.
    repo: String,
    /// Package path, without the leading `//`. Empty for the root package.
    package: String,
    /// Target name after the `:`.
    name: String,
}

impl TargetLabel {
    /// Parses a label, defaulting the name to the last package segment when the
    /// abbreviated form is used.
    pub fn parse(text: &str) -> Result<TargetLabel, LabelError> {
        let rest = match text.strip_prefix("@@").or_else(|| text.strip_prefix('@')) {
            // A repo sigil was present, so everything up to `//` names the repo.
            Some(after_sigil) => after_sigil,
            None => text,
        };
        let (repo, after_slashes) = if rest.len() < text.len() {
            rest.split_once("//").ok_or_else(|| LabelError::MissingPackage(text.to_owned()))?
        } else {
            let stripped = rest
                .strip_prefix("//")
                .ok_or_else(|| LabelError::MissingPackage(text.to_owned()))?;
            ("", stripped)
        };

        let (package, name) = match after_slashes.split_once(':') {
            Some((package, name)) => (package, name),
            None => {
                // `//java/com/acme/policy` means `//java/com/acme/policy:policy`.
                let last = after_slashes
                    .rsplit('/')
                    .next()
                    .filter(|it| !it.is_empty())
                    .ok_or_else(|| LabelError::MissingName(text.to_owned()))?;
                (after_slashes, last)
            }
        };

        if name.is_empty() {
            return Err(LabelError::MissingName(text.to_owned()));
        }
        Ok(TargetLabel {
            repo: repo.to_owned(),
            package: package.to_owned(),
            name: name.to_owned(),
        })
    }

    /// Repository name without sigils; empty for the main repo.
    pub fn repo(&self) -> &str {
        &self.repo
    }

    /// Package path without the leading `//`.
    pub fn package(&self) -> &str {
        &self.package
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    /// Whether this target lives in the repo under analysis, as opposed to an
    /// external dependency. Only workspace targets are worth watching for edits.
    pub fn is_main_repo(&self) -> bool {
        self.repo.is_empty()
    }

    /// The label a source file in this target's package would carry.
    ///
    /// Bazel names a source file by its package plus its path within that
    /// package, which is how a file gets resolved back to its owning target.
    pub fn sibling(&self, name: &str) -> TargetLabel {
        TargetLabel {
            repo: self.repo.clone(),
            package: self.package.clone(),
            name: name.to_owned(),
        }
    }
}

impl fmt::Display for TargetLabel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if !self.repo.is_empty() {
            write!(f, "@{}", self.repo)?;
        }
        write!(f, "//{}:{}", self.package, self.name)
    }
}

impl FromStr for TargetLabel {
    type Err = LabelError;
    fn from_str(s: &str) -> Result<TargetLabel, LabelError> {
        TargetLabel::parse(s)
    }
}

impl TryFrom<String> for TargetLabel {
    type Error = LabelError;
    fn try_from(s: String) -> Result<TargetLabel, LabelError> {
        TargetLabel::parse(&s)
    }
}

impl From<TargetLabel> for String {
    fn from(label: TargetLabel) -> String {
        label.to_string()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LabelError {
    /// No `//` separating repo from package.
    MissingPackage(String),
    /// Nothing after the `:`, and no package segment to default from.
    MissingName(String),
}

impl fmt::Display for LabelError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LabelError::MissingPackage(s) => write!(f, "label has no `//`: `{s}`"),
            LabelError::MissingName(s) => write!(f, "label has no target name: `{s}`"),
        }
    }
}

impl std::error::Error for LabelError {}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(s: &str) -> TargetLabel {
        TargetLabel::parse(s).unwrap_or_else(|e| panic!("`{s}` should parse: {e}"))
    }

    #[test]
    fn canonical_form_round_trips() {
        let l = parse("//java/com/acme/policy:policy");
        assert_eq!(l.repo(), "");
        assert_eq!(l.package(), "java/com/acme/policy");
        assert_eq!(l.name(), "policy");
        assert!(l.is_main_repo());
        assert_eq!(l.to_string(), "//java/com/acme/policy:policy");
    }

    #[test]
    fn abbreviated_form_defaults_the_name() {
        // Bazel prints and accepts both; they must intern to one label or the
        // index gets two shards for one target.
        assert_eq!(parse("//java/com/acme/policy"), parse("//java/com/acme/policy:policy"));
        assert_eq!(parse("//java/com/acme/policy").to_string(), "//java/com/acme/policy:policy");
    }

    #[test]
    fn bzlmod_double_sigil_is_the_main_repo() {
        // cquery prints main-repo labels as `@@//...`. Treating that as an
        // external repo would make every workspace target look like a
        // dependency, and nothing would ever be watched for edits.
        let l = parse("@@//java/com/acme/app:app");
        assert!(l.is_main_repo(), "@@// is the main repo");
        assert_eq!(l, parse("//java/com/acme/app:app"));
    }

    #[test]
    fn external_repos_are_distinguished() {
        let l = parse("@rules_java//java:defs");
        assert_eq!(l.repo(), "rules_java");
        assert_eq!(l.package(), "java");
        assert_eq!(l.name(), "defs");
        assert!(!l.is_main_repo());
        assert_eq!(l.to_string(), "@rules_java//java:defs");

        // The bzlmod canonical spelling carries a `+` in the repo name.
        let canonical = parse("@@rules_java+//toolchains:current_java_toolchain");
        assert_eq!(canonical.repo(), "rules_java+");
        assert!(!canonical.is_main_repo());
    }

    #[test]
    fn root_package_is_representable() {
        let l = parse("//:everything");
        assert_eq!(l.package(), "");
        assert_eq!(l.name(), "everything");
        assert_eq!(l.to_string(), "//:everything");
    }

    #[test]
    fn source_files_are_labels_too() {
        // This is how a file maps back to its target: same package, file name.
        let target = parse("//java/com/acme/policy:policy");
        let src = target.sibling("DefaultRetryPolicy.java");
        assert_eq!(src.to_string(), "//java/com/acme/policy:DefaultRetryPolicy.java");
        assert_eq!(src.package(), target.package());
    }

    #[test]
    fn malformed_labels_are_rejected() {
        use LabelError::*;
        assert!(matches!(TargetLabel::parse("java/com/acme"), Err(MissingPackage(_))));
        assert!(matches!(TargetLabel::parse("@repo/no/slashes"), Err(MissingPackage(_))));
        assert!(matches!(TargetLabel::parse("//pkg:"), Err(MissingName(_))));
        assert!(matches!(TargetLabel::parse("//"), Err(MissingName(_))));
        assert!(matches!(TargetLabel::parse(""), Err(MissingPackage(_))));
    }

    #[test]
    fn labels_survive_serde() {
        let l = parse("//java/com/acme/transport:transport");
        let json = serde_json::to_string(&l).unwrap();
        assert_eq!(json, "\"//java/com/acme/transport:transport\"");
        assert_eq!(serde_json::from_str::<TargetLabel>(&json).unwrap(), l);
    }

    #[test]
    fn ordering_groups_by_package() {
        // The index is keyed by label; a stable order keeps shard files
        // byte-identical between runs.
        let mut labels = [
            parse("//java/com/acme/util:util"),
            parse("//java/com/acme/core:core"),
            parse("@ext//a:a"),
            parse("//java/com/acme/core:other"),
        ];
        labels.sort();
        let rendered: Vec<_> = labels.iter().map(|it| it.to_string()).collect();
        assert_eq!(
            rendered,
            [
                "//java/com/acme/core:core",
                "//java/com/acme/core:other",
                "//java/com/acme/util:util",
                "@ext//a:a",
            ]
        );
    }
}
