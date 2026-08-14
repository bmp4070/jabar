//! Reading a target's compile inputs out of `bazel aquery --output=jsonproto`.
//!
//! # Why aquery rather than BSP
//!
//! The plan reached for the Build Server Protocol, on the assumption that
//! bazel-bsp is how you ask Bazel these questions. It is *a* way. It is also a
//! JVM tool that has to be installed and kept in step with the Bazel version,
//! and it answers by running an aspect over the workspace.
//!
//! `bazel aquery` asks the same question of the same Skyframe graph without any
//! of that, because it reports the javac action Bazel already planned. On the
//! fixture, one target's full compile inputs come back in ~170ms against a warm
//! server. BSP remains worth adding later — it brings `buildTarget/didChange`
//! push invalidation, which polling cannot match — so this module keeps its
//! parsing separate from [`crate::BazelCli`], leaving room for a second source
//! of the same [`CompileInfo`].
//!
//! # What is being parsed
//!
//! The action's `arguments` array is the literal javac command line. The flags
//! read here belong to Bazel's JavaBuilder, not to javac itself:
//!
//! ```text
//! --sources    a.java b.java        the target's own sources
//! --classpath  libdep-hjar.jar …    header jars of the direct dependencies
//! --output     libtarget.jar        the compiled output
//! ```
//!
//! Each takes a run of values terminated by the next `--flag`, which is why the
//! scan below collects until it sees one.
//!
//! Reading the command line is a real coupling to JavaBuilder's flag names, and
//! worth naming as such: if they change, this breaks. The alternative — walking
//! `inputDepSetIds` through `depSetOfFiles` and `pathFragments` — trades that
//! for a coupling to aquery's own graph encoding, and still cannot tell sources
//! from classpath without inspecting file extensions. The flags are the more
//! honest dependency, and [`ParseError::NoJavacAction`] makes a break loud.

use paths::Utf8PathBuf;
use rustc_hash::FxHashMap;
use serde::Deserialize;
use std::fmt;

use crate::TargetLabel;

/// A target's compile inputs, exactly as Bazel planned them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CompileInfo {
    pub label: TargetLabel,
    /// The target's own sources, relative to the workspace root.
    pub sources: Vec<Utf8PathBuf>,
    /// Header jars of the direct dependencies, relative to the exec root.
    ///
    /// These are the interesting artifact. A header jar holds class names,
    /// supertypes and method signatures with the method bodies stripped — which
    /// is an item tree, already built, already remotely cached, and already
    /// invalidated correctly by Bazel. What it does *not* carry is any source
    /// position: no `SourceFile`, no `LineNumberTable`. So it answers "does this
    /// symbol exist and what shape is it" but never "where is it".
    pub classpath: Vec<Utf8PathBuf>,
    /// The compiled output jar, relative to the exec root.
    pub output_jar: Option<Utf8PathBuf>,
}

impl CompileInfo {
    /// Sources ending in `.java`. Bazel targets can carry generated sources and
    /// resources through the same flag.
    pub fn java_sources(&self) -> impl Iterator<Item = &Utf8PathBuf> {
        self.sources.iter().filter(|p| p.extension() == Some("java"))
    }
}

/// The subset of the aquery document this cares about.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct AqueryDocument {
    #[serde(default)]
    actions: Vec<Action>,
    #[serde(default)]
    targets: Vec<Target>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Action {
    #[serde(default)]
    mnemonic: String,
    #[serde(default)]
    target_id: u32,
    #[serde(default)]
    arguments: Vec<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Target {
    id: u32,
    label: String,
}

/// Parses every Javac action in an aquery document.
///
/// Targets with no Javac action — a `java_import` wrapping a prebuilt jar, or a
/// target that was filtered out — simply do not appear in the result.
pub fn parse_javac_actions(json: &str) -> Result<Vec<CompileInfo>, ParseError> {
    let doc: AqueryDocument = serde_json::from_str(json).map_err(ParseError::Malformed)?;

    let labels: FxHashMap<u32, &str> =
        doc.targets.iter().map(|t| (t.id, t.label.as_str())).collect();

    let mut out = Vec::new();
    for action in doc.actions.iter().filter(|a| a.mnemonic == "Javac") {
        let raw_label = labels
            .get(&action.target_id)
            .ok_or(ParseError::UnknownTarget { target_id: action.target_id })?;
        let label = TargetLabel::parse(raw_label)
            .map_err(|e| ParseError::BadLabel { label: (*raw_label).to_owned(), source: e })?;

        let sources = collect_flag(&action.arguments, "--sources");
        let classpath = collect_flag(&action.arguments, "--classpath");
        let output_jar = collect_flag(&action.arguments, "--output").into_iter().next();

        // If none of the flags are present but the command line points at a
        // params file, the flags are in that file and this parse found nothing
        // — which would otherwise be indistinguishable from a target that
        // genuinely has no sources. Refuse instead.
        if sources.is_empty()
            && classpath.is_empty()
            && output_jar.is_none()
            && let Some(params) = params_file(&action.arguments)
        {
            return Err(ParseError::ParamsFile {
                label: label.to_string(),
                params_file: params.to_owned(),
            });
        }

        out.push(CompileInfo { label, sources, classpath, output_jar });
    }

    if out.is_empty() && !doc.actions.is_empty() {
        return Err(ParseError::NoJavacAction);
    }
    Ok(out)
}

/// Collects the run of values following `flag`, stopping at the next `--flag`
/// or at a params-file reference.
///
/// Only the first occurrence is read: JavaBuilder passes each of these once, and
/// silently concatenating repeats would hide a command line we do not understand.
///
/// The `@` stop matters as much as the `--` one: a params file is never a value
/// of `--sources`, and treating one as a source path would put a `.params` file
/// into the compile inputs.
fn collect_flag(args: &[String], flag: &str) -> Vec<Utf8PathBuf> {
    let Some(start) = args.iter().position(|a| a == flag) else {
        return Vec::new();
    };
    args[start + 1..]
        .iter()
        .take_while(|a| !a.starts_with("--") && !a.starts_with('@'))
        .map(Utf8PathBuf::from)
        .collect()
}

/// The `@file` argument in a command line, if there is one.
///
/// Bazel moves a javac command line into a params file once it grows past a
/// size threshold — a large classpath does it easily — and persistent workers
/// use one unconditionally. The flags then live in the file rather than the
/// argv.
fn params_file(args: &[String]) -> Option<&str> {
    args.iter().find_map(|a| a.strip_prefix('@')).filter(|it| !it.is_empty())
}

#[derive(Debug)]
pub enum ParseError {
    Malformed(serde_json::Error),
    /// An action referenced a target id the document did not define.
    UnknownTarget {
        target_id: u32,
    },
    BadLabel {
        label: String,
        source: crate::LabelError,
    },
    /// Actions were present but none was a Javac action, which usually means
    /// the query matched something unexpected rather than that the target has
    /// no sources.
    NoJavacAction,
    /// The command line is a params-file reference, so the flags this parser
    /// reads are not in the argv.
    ///
    /// Reported rather than tolerated: an empty [`CompileInfo`] here would say
    /// "this target has no sources and no classpath", which is a confident
    /// wrong answer of exactly the kind this project exists to avoid. Reading
    /// the file is the real fix; see the note in `docs/phase-1.md` §5.
    ParamsFile {
        label: String,
        params_file: String,
    },
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ParseError::Malformed(e) => write!(f, "aquery output is not valid JSON: {e}"),
            ParseError::UnknownTarget { target_id } => {
                write!(f, "action references undefined target id {target_id}")
            }
            ParseError::BadLabel { label, source } => {
                write!(f, "aquery returned an unparseable label `{label}`: {source}")
            }
            ParseError::NoJavacAction => {
                f.write_str("aquery returned actions but no Javac action among them")
            }
            ParseError::ParamsFile { label, params_file } => write!(
                f,
                "javac for `{label}` reads its flags from the params file `{params_file}`, \
                 which this parser does not yet read; refusing to report empty sources"
            ),
        }
    }
}

impl std::error::Error for ParseError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            ParseError::Malformed(e) => Some(e),
            ParseError::BadLabel { source, .. } => Some(source),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Real `bazel aquery --output=jsonproto` output for
    /// `//java/com/acme/service/order:order` in `fixtures/megarepo`, trimmed to
    /// the fields this parser reads.
    const REAL_AQUERY: &str = include_str!("../test_data/javac_aquery.json");

    fn parse_one(json: &str) -> CompileInfo {
        let mut infos = parse_javac_actions(json).expect("should parse");
        assert_eq!(infos.len(), 1, "expected one Javac action");
        infos.pop().unwrap()
    }

    #[test]
    fn reads_real_bazel_output() {
        let info = parse_one(REAL_AQUERY);
        assert_eq!(info.label.to_string(), "//java/com/acme/service/order:order");

        let sources: Vec<_> = info.java_sources().map(|p| p.as_str()).collect();
        assert_eq!(
            sources,
            [
                "java/com/acme/service/order/OrderRepository.java",
                "java/com/acme/service/order/OrderService.java",
            ]
        );

        // Six direct dependencies, and every one arrives as a header jar rather
        // than a full class jar.
        assert_eq!(info.classpath.len(), 6);
        assert!(
            info.classpath.iter().all(|p| {
                let name = p.file_name().unwrap_or_default();
                name.ends_with("-hjar.jar") || name.ends_with("-ijar.jar")
            }),
            "expected only header jars, got {:?}",
            info.classpath
        );
        assert!(
            info.classpath.iter().any(|p| p.as_str().contains("tinyjson")),
            "the binary-only dependency should be on the classpath"
        );

        assert_eq!(
            info.output_jar.as_deref().map(|p| p.as_str()),
            Some("bazel-out/darwin_arm64-fastbuild/bin/java/com/acme/service/order/liborder.jar")
        );
    }

    #[test]
    fn flag_runs_stop_at_the_next_flag() {
        let args: Vec<String> =
            ["--classpath", "a.jar", "b.jar", "--sources", "X.java", "--output", "out.jar"]
                .iter()
                .map(|s| s.to_string())
                .collect();
        assert_eq!(collect_flag(&args, "--classpath"), ["a.jar", "b.jar"]);
        assert_eq!(collect_flag(&args, "--sources"), ["X.java"]);
        assert_eq!(collect_flag(&args, "--output"), ["out.jar"]);
        assert!(collect_flag(&args, "--missing").is_empty());
    }

    #[test]
    fn a_flag_with_no_values_yields_nothing() {
        let args: Vec<String> =
            ["--classpath", "--sources", "X.java"].iter().map(|s| s.to_string()).collect();
        assert!(collect_flag(&args, "--classpath").is_empty());
        assert_eq!(collect_flag(&args, "--sources"), ["X.java"]);
    }

    #[test]
    fn a_trailing_flag_yields_nothing() {
        let args: Vec<String> =
            ["--sources", "X.java", "--output"].iter().map(|s| s.to_string()).collect();
        assert!(collect_flag(&args, "--output").is_empty());
    }

    #[test]
    fn targets_without_sources_are_still_valid() {
        // A java_library with only exports has a Javac action and no --sources.
        let json = r#"{
          "actions": [{"mnemonic": "Javac", "targetId": 1,
                       "arguments": ["--output", "libx.jar"]}],
          "targets": [{"id": 1, "label": "//x:x"}]
        }"#;
        let info = parse_one(json);
        assert!(info.sources.is_empty());
        assert!(info.classpath.is_empty(), "a target with no deps has an empty classpath");
        assert_eq!(info.output_jar.as_deref().map(|p| p.as_str()), Some("libx.jar"));
    }

    #[test]
    fn non_javac_actions_are_ignored() {
        let json = r#"{
          "actions": [
            {"mnemonic": "JavaSourceJar", "targetId": 1, "arguments": ["--output", "src.jar"]},
            {"mnemonic": "Javac", "targetId": 1, "arguments": ["--output", "libx.jar"]}
          ],
          "targets": [{"id": 1, "label": "//x:x"}]
        }"#;
        assert_eq!(parse_one(json).output_jar.as_deref().map(|p| p.as_str()), Some("libx.jar"));
    }

    #[test]
    fn an_empty_document_is_not_an_error() {
        // A java_import has no Javac action at all. That is a fact about the
        // target, not a failure to report.
        let empty = parse_javac_actions(r#"{}"#).expect("empty aquery output is legitimate");
        assert!(empty.is_empty());
    }

    #[test]
    fn actions_without_a_javac_one_are_an_error() {
        // Something matched, but not what was asked for. Silently returning
        // nothing here would look identical to "this target has no sources".
        let json = r#"{
          "actions": [{"mnemonic": "Turbine", "targetId": 1, "arguments": []}],
          "targets": [{"id": 1, "label": "//x:x"}]
        }"#;
        assert!(matches!(parse_javac_actions(json), Err(ParseError::NoJavacAction)));
    }

    #[test]
    fn a_dangling_target_id_is_an_error() {
        let json = r#"{
          "actions": [{"mnemonic": "Javac", "targetId": 9, "arguments": []}],
          "targets": [{"id": 1, "label": "//x:x"}]
        }"#;
        assert!(matches!(
            parse_javac_actions(json),
            Err(ParseError::UnknownTarget { target_id: 9 })
        ));
    }

    #[test]
    fn an_unparseable_label_is_an_error() {
        let json = r#"{
          "actions": [{"mnemonic": "Javac", "targetId": 1, "arguments": []}],
          "targets": [{"id": 1, "label": "not-a-label"}]
        }"#;
        assert!(matches!(parse_javac_actions(json), Err(ParseError::BadLabel { .. })));
    }

    #[test]
    fn a_params_file_command_line_is_refused_not_read_as_empty() {
        // The scale break: past a size threshold, or under a persistent worker,
        // Bazel moves the flags into an @file. Returning an empty CompileInfo
        // would claim the target has no sources -- a confident wrong answer.
        let json = r#"{
          "actions": [{"mnemonic": "Javac", "targetId": 1,
                       "arguments": ["external/…/JavaBuilder", "@bazel-out/x/libfoo.jar-2.params"]}],
          "targets": [{"id": 1, "label": "//x:x"}]
        }"#;
        let err = parse_javac_actions(json).expect_err("should refuse");
        let ParseError::ParamsFile { label, params_file } = &err else {
            panic!("expected ParamsFile, got {err:?}");
        };
        assert_eq!(label, "//x:x");
        assert_eq!(params_file, "bazel-out/x/libfoo.jar-2.params");
        // The message has to name the file, or the failure is unactionable.
        assert!(err.to_string().contains("libfoo.jar-2.params"), "{err}");
    }

    #[test]
    fn an_inline_command_line_is_unaffected_by_the_guard() {
        // A target with a genuinely empty classpath and no @file is still fine.
        let json = r#"{
          "actions": [{"mnemonic": "Javac", "targetId": 1,
                       "arguments": ["--sources", "A.java"]}],
          "targets": [{"id": 1, "label": "//x:x"}]
        }"#;
        assert_eq!(parse_one(json).sources.len(), 1);
    }

    #[test]
    fn a_params_file_is_never_taken_as_a_flag_value() {
        // A `.params` path landing in `sources` would put it on the compile
        // inputs. The value run stops at `@` as well as at the next `--flag`.
        let json = r#"{
          "actions": [{"mnemonic": "Javac", "targetId": 1,
                       "arguments": ["--sources", "A.java", "@bazel-out/extra.params"]}],
          "targets": [{"id": 1, "label": "//x:x"}]
        }"#;
        let info = parse_one(json);
        assert_eq!(info.sources.len(), 1);
    }

    #[test]
    fn params_file_detection() {
        let args = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(params_file(&args(&["javac", "@a.params"])), Some("a.params"));
        assert_eq!(params_file(&args(&["javac", "--sources", "A.java"])), None);
        assert_eq!(params_file(&args(&["@"])), None, "a bare @ is not a file");
    }

    #[test]
    fn malformed_json_is_reported_not_panicked() {
        assert!(matches!(parse_javac_actions("{ this is not json"), Err(ParseError::Malformed(_))));
    }

    #[test]
    fn multiple_targets_are_kept_apart() {
        let json = r#"{
          "actions": [
            {"mnemonic": "Javac", "targetId": 1, "arguments": ["--sources", "A.java"]},
            {"mnemonic": "Javac", "targetId": 2, "arguments": ["--sources", "B.java"]}
          ],
          "targets": [{"id": 1, "label": "//a:a"}, {"id": 2, "label": "//b:b"}]
        }"#;
        let infos = parse_javac_actions(json).unwrap();
        assert_eq!(infos.len(), 2);
        assert_eq!(infos[0].label.to_string(), "//a:a");
        assert_eq!(infos[1].label.to_string(), "//b:b");
        assert_eq!(infos[1].sources[0].as_str(), "B.java");
    }

    #[test]
    fn non_java_sources_are_filtered_by_java_sources() {
        let json = r#"{
          "actions": [{"mnemonic": "Javac", "targetId": 1,
                       "arguments": ["--sources", "A.java", "gen.srcjar", "R.txt"]}],
          "targets": [{"id": 1, "label": "//x:x"}]
        }"#;
        let info = parse_one(json);
        assert_eq!(info.sources.len(), 3, "everything passed to --sources is kept");
        let java: Vec<_> = info.java_sources().map(|p| p.as_str()).collect();
        assert_eq!(java, ["A.java"], "but only .java files are parseable source");
    }
}
