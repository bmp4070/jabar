//! Asking the `bazel` CLI about the workspace.
//!
//! # Finding the owning package without asking Bazel
//!
//! The obvious way to resolve a file to its target is to hand Bazel the path
//! and let it work out the package. That works, but it fails badly at the edge
//! that matters: a file in no package at all — a scratch file, a half-deleted
//! source, anything an agent just created — makes `bazel query` exit non-zero
//! with
//!
//! ```text
//! ERROR: no such target '//:java/com/acme/orphan/NotInAnyTarget.java' … however,
//! a source file of this name exists.
//! ```
//!
//! Telling that apart from a real failure means pattern-matching Bazel's error
//! prose, which is exactly the kind of coupling that breaks on upgrade. Worse,
//! getting it wrong turns "this file is not in a target" into "the build system
//! is broken", or the reverse.
//!
//! A Bazel package is a directory holding a `BUILD` or `BUILD.bazel` file, and a
//! source belongs to the nearest such ancestor. That is a handful of `stat`
//! calls, so [`BazelCli::package_of`] answers it directly and orphans never
//! reach Bazel at all. Files in no package are then an ordinary `Ok(None)`
//! rather than an error to be decoded.

use std::process::Command;

use paths::{AbsPath, AbsPathBuf, Utf8Path, Utf8PathBuf};

use crate::aquery::{self, CompileInfo};
use crate::label::TargetLabel;

/// Names Bazel recognises as declaring a package, in the order it checks them.
const BUILD_FILE_NAMES: [&str; 2] = ["BUILD.bazel", "BUILD"];

/// Bazel's exit code for "finished, but something in the request did not
/// resolve" — what `--keep_going` produces instead of failing outright.
const EXIT_PARTIAL_SUCCESS: i32 = 3;

/// Runs `bazel` in a workspace.
#[derive(Clone, Debug)]
pub struct BazelCli {
    workspace_root: AbsPathBuf,
    program: String,
    output_base: Option<Utf8PathBuf>,
}

impl BazelCli {
    /// `workspace_root` must be the directory holding `MODULE.bazel` or
    /// `WORKSPACE`; it is where `bazel` will be run and what paths are
    /// interpreted relative to.
    pub fn new(workspace_root: AbsPathBuf) -> BazelCli {
        BazelCli { workspace_root, program: "bazel".to_owned(), output_base: None }
    }

    /// Runs bazel against its own output base rather than the workspace default.
    ///
    /// Bazel takes an exclusive lock per output base, so sharing one means
    /// jabar's invocations queue behind the user's builds *and* block them. A
    /// separate base removes that entirely.
    ///
    /// It is not free, which is why this is opt-in rather than the default: a
    /// second base is a second analysis universe and a second set of outputs,
    /// which on a large repo is gigabytes and a full cold analysis the first
    /// time. Sharing is right when nothing else is building concurrently, and
    /// wrong the moment something is.
    pub fn with_output_base(mut self, output_base: Option<Utf8PathBuf>) -> BazelCli {
        self.output_base = output_base;
        self
    }

    /// The output base in use, or `None` when sharing the workspace default.
    pub fn output_base(&self) -> Option<&Utf8Path> {
        self.output_base.as_deref()
    }

    /// Overrides the executable, for `bazelisk` or an absolute path.
    pub fn with_program(mut self, program: impl Into<String>) -> BazelCli {
        self.program = program.into();
        self
    }

    /// The full argument list, with any startup options ahead of the command.
    ///
    /// `--output_base` is a *startup* option: bazel rejects it outright if it
    /// appears after the command, so position is correctness rather than style.
    fn argv(&self, args: &[&str]) -> Vec<String> {
        let mut argv = Vec::with_capacity(args.len() + 1);
        if let Some(base) = &self.output_base {
            argv.push(format!("--output_base={base}"));
        }
        argv.extend(args.iter().map(|a| (*a).to_string()));
        argv
    }

    pub fn workspace_root(&self) -> &AbsPath {
        &self.workspace_root
    }

    /// The Bazel package containing `file`, as a workspace-relative path.
    ///
    /// Returns `None` when no ancestor up to the workspace root declares a
    /// package, which is the orphan case. Does not run Bazel.
    pub fn package_of(&self, file: &AbsPath) -> Option<String> {
        let relative = file.strip_prefix(&self.workspace_root)?;
        // Start at the file's directory and walk up, staying inside the
        // workspace. The nearest BUILD file wins, which is Bazel's own rule and
        // is what makes a nested package like `service/order` beat `service`.
        let mut dir = relative.parent();
        while let Some(current) = dir {
            let abs = if current.as_str().is_empty() {
                self.workspace_root.clone()
            } else {
                self.workspace_root.join(current)
            };
            if BUILD_FILE_NAMES.iter().any(|name| abs.join(name).as_utf8_path().is_file()) {
                return Some(current.as_str().to_owned());
            }
            dir = current.parent();
        }
        None
    }

    /// The target that compiles `file`, if any.
    ///
    /// `Ok(None)` covers both "not in any package" and "in a package but not in
    /// any target's `srcs`" — a distinction Bazel does not draw either, and one
    /// no caller has yet needed.
    pub fn owning_target(&self, file: &AbsPath) -> Result<Option<TargetLabel>, BazelError> {
        let Some(package) = self.package_of(file) else {
            tracing::debug!(%file, "file is in no bazel package");
            return Ok(None);
        };
        let relative = file
            .strip_prefix(&self.workspace_root)
            .expect("package_of already established the prefix");
        let within_package = strip_package_prefix(relative, &package);
        let source_label = format!("//{package}:{within_package}");
        check_query_safe(&source_label)?;

        // `--keep_going` turns "that source is not a declared target" from a
        // hard exit 7 into exit 3 with empty output. That matters: a file can
        // sit inside a package — the fixture's orphan is in the root package —
        // while belonging to no target at all. Without this, an unowned file
        // and a broken workspace are the same error.
        let query = format!("same_pkg_direct_rdeps({source_label})");
        let stdout = self.run_allowing(
            &["query", &query, "--output=label", "--keep_going", "--noshow_progress"],
            &[EXIT_PARTIAL_SUCCESS],
        )?;

        // A source can feed several targets — a library and its test. Taking the
        // first in Bazel's own ordering keeps the choice deterministic across
        // runs, which matters more than which one wins.
        match stdout.lines().map(str::trim).find(|l| !l.is_empty()) {
            Some(line) => TargetLabel::parse(line)
                .map(Some)
                .map_err(|source| BazelError::BadLabel { label: line.to_owned(), source }),
            None => Ok(None),
        }
    }

    /// The compile inputs Bazel planned for `target`: sources, header-jar
    /// classpath, and output jar.
    pub fn compile_info(&self, target: &TargetLabel) -> Result<Option<CompileInfo>, BazelError> {
        let rendered = target.to_string();
        check_query_safe(&rendered)?;

        // No `--keep_going` here, and no event filtering. A target that does not
        // exist must surface as an error rather than as "no sources", and
        // `--ui_event_filters` would take the jsonproto payload with it: aquery
        // writes its result to stdout as an event.
        let query = format!("mnemonic(\"Javac\", {rendered})");
        let stdout = self.run(&["aquery", &query, "--output=jsonproto", "--noshow_progress"])?;

        let mut infos = aquery::parse_javac_actions(&stdout)
            .map_err(|source| BazelError::Aquery { target: rendered, source })?;
        // A `java_import` has no Javac action; that is a fact about the target,
        // not a failure.
        Ok(infos.pop())
    }

    fn run(&self, args: &[&str]) -> Result<String, BazelError> {
        self.run_allowing(args, &[])
    }

    /// Runs `bazel`, treating `also_ok` exit codes as success alongside 0.
    fn run_allowing(&self, args: &[&str], also_ok: &[i32]) -> Result<String, BazelError> {
        let span = tracing::debug_span!("bazel", program = %self.program, ?args);
        let _enter = span.enter();

        // Arguments are passed as a vector, never through a shell, so nothing in
        // a path can start a second command. `check_query_safe` covers the
        // remaining risk, which is a path altering the *query* expression.
        let output = Command::new(&self.program)
            .args(self.argv(args))
            .current_dir(self.workspace_root.as_str())
            .output()
            .map_err(|source| BazelError::Spawn { program: self.program.clone(), source })?;

        let code = output.status.code();
        let tolerated = code.is_some_and(|c| also_ok.contains(&c));
        if !output.status.success() && !tolerated {
            let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
            tracing::debug!(?code, %stderr, "bazel failed");
            return Err(BazelError::CommandFailed {
                args: args.iter().map(|s| (*s).to_owned()).collect(),
                code,
                stderr,
            });
        }
        if tolerated {
            tracing::debug!(?code, "bazel reported partial success");
        }
        String::from_utf8(output.stdout).map_err(|_| BazelError::NonUtf8Output)
    }
}

/// Removes the package prefix from a workspace-relative path.
fn strip_package_prefix<'a>(relative: &'a Utf8Path, package: &str) -> &'a str {
    let relative = relative.as_str();
    if package.is_empty() {
        return relative;
    }
    relative.strip_prefix(package).and_then(|rest| rest.strip_prefix('/')).unwrap_or(relative)
}

/// Refuses label text that could change the shape of a query expression.
///
/// Arguments never reach a shell, so this is not about shell metacharacters. It
/// is about a filename containing `)` or `"` ending the query early and turning
/// a lookup into a different, valid, wrong query. Bazel labels have a restricted
/// character set anyway, so rejecting the rest costs nothing real.
fn check_query_safe(label: &str) -> Result<(), BazelError> {
    const FORBIDDEN: [char; 8] = ['(', ')', '"', '\'', '\\', '\n', '\r', ' '];
    if let Some(bad) = label.chars().find(|c| FORBIDDEN.contains(c)) {
        return Err(BazelError::UnsafeLabel { label: label.to_owned(), character: bad });
    }
    Ok(())
}

#[derive(Debug)]
pub enum BazelError {
    /// `bazel` could not be started. Usually it is not installed.
    Spawn {
        program: String,
        source: std::io::Error,
    },
    CommandFailed {
        args: Vec<String>,
        code: Option<i32>,
        stderr: String,
    },
    /// Bazel printed something that was not UTF-8.
    NonUtf8Output,
    BadLabel {
        label: String,
        source: crate::LabelError,
    },
    Aquery {
        target: String,
        source: aquery::ParseError,
    },
    /// A path held a character that could alter the query expression.
    UnsafeLabel {
        label: String,
        character: char,
    },
}

impl std::fmt::Display for BazelError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BazelError::Spawn { program, source } => {
                write!(f, "could not run `{program}`: {source}")
            }
            BazelError::CommandFailed { args, code, stderr } => {
                let code = code.map(|c| c.to_string()).unwrap_or_else(|| "signal".to_owned());
                write!(f, "`bazel {}` exited {code}: {stderr}", args.join(" "))
            }
            BazelError::NonUtf8Output => f.write_str("bazel produced non-UTF-8 output"),
            BazelError::BadLabel { label, source } => {
                write!(f, "bazel returned an unparseable label `{label}`: {source}")
            }
            BazelError::Aquery { target, source } => {
                write!(f, "could not read compile info for `{target}`: {source}")
            }
            BazelError::UnsafeLabel { label, character } => {
                write!(f, "refusing to query label containing `{character}`: `{label}`")
            }
        }
    }
}

impl std::error::Error for BazelError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            BazelError::Spawn { source, .. } => Some(source),
            BazelError::BadLabel { source, .. } => Some(source),
            BazelError::Aquery { source, .. } => Some(source),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn abs(s: &str) -> AbsPathBuf {
        AbsPathBuf::try_from(s).expect("test path should be absolute")
    }

    #[test]
    fn package_prefix_stripping() {
        assert_eq!(
            strip_package_prefix(
                Utf8Path::new("java/com/acme/policy/P.java"),
                "java/com/acme/policy"
            ),
            "P.java"
        );
        // Root package: the whole relative path is the target name.
        assert_eq!(strip_package_prefix(Utf8Path::new("Top.java"), ""), "Top.java");
        // A source nested below the package directory keeps its subpath.
        assert_eq!(
            strip_package_prefix(Utf8Path::new("pkg/sub/dir/File.java"), "pkg"),
            "sub/dir/File.java"
        );
    }

    #[test]
    fn query_injection_is_refused() {
        // A filename cannot reach a shell, but it can still end a query
        // expression early and turn a lookup into a different valid query.
        assert!(check_query_safe("//java/com/acme:Fine.java").is_ok());
        for hostile in
            ["//x:a).java", "//x:a(b.java", "//x:a\".java", "//x:a b.java", "//x:a\nunion //..."]
        {
            assert!(
                matches!(check_query_safe(hostile), Err(BazelError::UnsafeLabel { .. })),
                "should have refused `{hostile}`"
            );
        }
    }

    #[test]
    fn a_missing_bazel_is_reported_not_panicked() {
        let cli = BazelCli::new(abs("/nonexistent-workspace"))
            .with_program("definitely-not-a-real-bazel-binary");
        let err = cli
            .compile_info(&TargetLabel::parse("//x:x").unwrap())
            .expect_err("should fail when the binary is missing");
        assert!(matches!(err, BazelError::Spawn { .. }), "got {err:?}");
        // The message has to name the program, or a missing bazel looks like a
        // broken workspace.
        assert!(err.to_string().contains("definitely-not-a-real-bazel-binary"));
    }

    #[test]
    fn startup_options_precede_the_command() {
        // bazel rejects `--output_base` after the command outright, so this is
        // correctness rather than tidiness.
        let cli = BazelCli::new(abs("/repo"))
            .with_output_base(Some(Utf8PathBuf::from("/tmp/jabar-base")));
        let argv = cli.argv(&["query", "//..."]);
        assert_eq!(argv, ["--output_base=/tmp/jabar-base", "query", "//..."]);
        assert_eq!(cli.output_base().map(|p| p.as_str()), Some("/tmp/jabar-base"));
    }

    #[test]
    fn sharing_the_default_base_adds_no_arguments() {
        // The default. Nothing is passed, so bazel behaves exactly as the user's
        // own invocations do -- same server, same cache, same lock.
        let cli = BazelCli::new(abs("/repo"));
        assert_eq!(cli.argv(&["query", "//..."]), ["query", "//..."]);
        assert_eq!(cli.output_base(), None);
    }

    #[test]
    fn paths_outside_the_workspace_have_no_package() {
        let cli = BazelCli::new(abs("/repo"));
        assert_eq!(
            cli.package_of(AbsPath::new_unchecked(Utf8Path::new("/elsewhere/A.java"))),
            None
        );
    }
}
