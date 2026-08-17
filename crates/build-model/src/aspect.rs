//! Running the SCIP aspect to produce an index.
//!
//! Bazel loads an aspect by label, so the `.bzl` file has to live inside the
//! workspace being indexed. jabar ships the file (see `aspects/`) and writes it
//! into `.jabar/aspects/` on demand — a directory named for its owner, so it is
//! obvious what wrote it and safe to delete.
//!
//! # The build-first prerequisite
//!
//! The aspect writes its outputs relative to `sourceroot` and expects
//! `sourceroot/bazel-out` to already be the convenience symlink into the exec
//! root. On a workspace nothing has built, that symlink does not exist, the
//! aspect creates a real directory in its place, and every output lands
//! somewhere Bazel will not look — reported as `output '...scip' was not
//! created`, which does not name the cause. [`AspectRunner::run`] therefore
//! checks for the symlink and says so plainly rather than letting that error
//! surface.

use std::process::Command;

use paths::{AbsPath, AbsPathBuf, Utf8Path, Utf8PathBuf};

use crate::bazel::BazelError;

/// The aspect source, shipped in the binary so there is nothing to install.
const ASPECT_SOURCE: &str = include_str!("../aspects/scip_java.bzl");

/// Where the aspect is written inside the workspace, relative to its root.
const ASPECT_DIR: &str = ".jabar/aspects";
const ASPECT_LABEL: &str = "//.jabar/aspects:scip_java.bzl%scip_java_aspect";

/// What to index and how.
#[derive(Clone, Debug)]
pub struct AspectConfig {
    /// Target pattern to index.
    ///
    /// Not `//...` by default, and deliberately so: on a real megarepo that
    /// pattern includes targets broken at HEAD, targets needing credentials,
    /// and targets whose toolchains are not installed. Scoping is the normal
    /// case, not the exception.
    pub targets: Vec<String>,
    /// Absolute path to the `scip-java` binary.
    pub scip_java: Utf8PathBuf,
    /// `JAVA_HOME` for the indexer, which requires it explicitly.
    pub java_home: Utf8PathBuf,
}

/// Installs and runs the aspect.
pub struct AspectRunner<'a> {
    workspace_root: &'a AbsPath,
    program: &'a str,
    output_base: Option<&'a Utf8Path>,
}

impl<'a> AspectRunner<'a> {
    pub fn new(
        workspace_root: &'a AbsPath,
        program: &'a str,
        output_base: Option<&'a Utf8Path>,
    ) -> AspectRunner<'a> {
        AspectRunner { workspace_root, program, output_base }
    }

    /// Writes the aspect into the workspace, returning whether anything changed.
    ///
    /// Rewritten whenever it differs, so upgrading jabar upgrades the aspect
    /// rather than silently running an old one against a new server.
    pub fn install(&self) -> std::io::Result<bool> {
        let dir = self.workspace_root.join(ASPECT_DIR);
        std::fs::create_dir_all(dir.as_str())?;

        let build_file = dir.join("BUILD.bazel");
        let expected_build = "exports_files([\"scip_java.bzl\"])\n";
        let aspect_file = dir.join("scip_java.bzl");

        let current = std::fs::read_to_string(aspect_file.as_str()).unwrap_or_default();
        if current == ASPECT_SOURCE
            && std::fs::read_to_string(build_file.as_str()).unwrap_or_default() == expected_build
        {
            return Ok(false);
        }
        std::fs::write(aspect_file.as_str(), ASPECT_SOURCE)?;
        std::fs::write(build_file.as_str(), expected_build)?;
        tracing::info!(dir = %dir, "installed the SCIP aspect");
        Ok(true)
    }

    /// Builds the SCIP output group, producing one shard per Java target.
    ///
    /// Returns the directory the shards landed in.
    pub fn run(&self, config: &AspectConfig) -> Result<AbsPathBuf, AspectError> {
        let bazel_bin = self.workspace_root.join("bazel-bin");
        // See the module docs: without the symlink the outputs vanish and
        // Bazel's own error does not explain why.
        if !bazel_bin.as_utf8_path().is_symlink() {
            return Err(AspectError::NotBuilt);
        }

        self.install().map_err(AspectError::Install)?;

        // Switching output base leaves `bazel-bin` pointing into the *previous*
        // one until a command rewrites it, so the aspect writes where Bazel is
        // no longer looking and the run fails once. Bazel repoints the symlink
        // as it goes, so the retry succeeds. Detected rather than always
        // retried, so a genuine failure still fails once and fast.
        let stale_symlink = self.output_base.is_some_and(|base| {
            std::fs::read_link(bazel_bin.as_str())
                .map(|target| !target.starts_with(base.as_std_path()))
                .unwrap_or(false)
        });

        let mut args: Vec<String> = Vec::new();
        if let Some(base) = self.output_base {
            // A startup option: bazel rejects it after the command.
            args.push(format!("--output_base={base}"));
        }
        args.push("build".to_owned());
        args.extend(config.targets.iter().cloned());
        args.extend([
            format!("--aspects={ASPECT_LABEL}"),
            "--output_groups=scip".to_owned(),
            // A repo of any size has targets that will not build here. Indexing
            // the ones that do beats indexing none.
            "--keep_going".to_owned(),
            "--noshow_progress".to_owned(),
            format!("--define=sourceroot={}", self.workspace_root),
            format!("--define=java_home={}", config.java_home),
            format!("--define=scip_java_binary={}", config.scip_java),
        ]);

        tracing::info!(targets = ?config.targets, "running the SCIP aspect");
        let mut output = self.run_once(&args)?;
        if !output.status.success() && stale_symlink {
            tracing::info!(
                "the first run repointed `bazel-bin` at the configured output base; retrying"
            );
            output = self.run_once(&args)?;
        }

        let code = output.status.code();
        // Exit 3 is `--keep_going`'s "finished, but something did not build",
        // which is the expected outcome on a repo with targets we cannot
        // compile. Anything else that failed is a real failure.
        if !output.status.success() && code != Some(3) {
            let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
            return Err(AspectError::Bazel(BazelError::CommandFailed { args, code, stderr }));
        }
        if code == Some(3) {
            tracing::info!("some targets did not build; indexing what did");
        }
        Ok(bazel_bin)
    }

    /// Runs the aspect, retrying once if the failure was a stale symlink.
    fn run_once(&self, args: &[String]) -> Result<std::process::Output, AspectError> {
        Command::new(self.program)
            .args(args)
            .current_dir(self.workspace_root.as_str())
            .output()
            .map_err(|source| {
                AspectError::Bazel(BazelError::Spawn { program: self.program.to_owned(), source })
            })
    }
}

#[derive(Debug)]
pub enum AspectError {
    /// The workspace has never been built, so `bazel-bin` is not yet a symlink.
    NotBuilt,
    Install(std::io::Error),
    Bazel(BazelError),
}

impl std::fmt::Display for AspectError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AspectError::NotBuilt => f.write_str(
                "the workspace has no `bazel-bin` symlink yet, so aspect outputs would be \
                 written where Bazel cannot see them; run any `bazel build` once first",
            ),
            AspectError::Install(err) => write!(f, "could not write the aspect: {err}"),
            AspectError::Bazel(err) => write!(f, "{err}"),
        }
    }
}

impl std::error::Error for AspectError {}

#[cfg(test)]
mod tests {
    use super::*;

    fn root(dir: &tempfile::TempDir) -> AbsPathBuf {
        AbsPathBuf::try_from(dir.path().to_str().expect("utf-8 temp path"))
            .expect("temp dirs are absolute")
    }

    #[test]
    fn installing_writes_the_aspect_and_its_build_file() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = root(&dir);
        let runner = AspectRunner::new(&root, "bazel", None);

        assert!(runner.install().expect("install"), "first install writes");
        let aspect = dir.path().join(".jabar/aspects/scip_java.bzl");
        assert!(aspect.exists());
        assert!(dir.path().join(".jabar/aspects/BUILD.bazel").exists());
        // The shipped source, not a stub.
        let written = std::fs::read_to_string(&aspect).expect("read");
        assert!(written.contains("scip_java_aspect"), "wrote the real aspect");
        assert!(written.contains("JABAR:"), "including our Bazel 9 fixes");
    }

    #[test]
    fn installing_twice_is_a_no_op() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = root(&dir);
        let runner = AspectRunner::new(&root, "bazel", None);
        assert!(runner.install().expect("install"));
        assert!(!runner.install().expect("install"), "unchanged, so not rewritten");
    }

    #[test]
    fn a_stale_aspect_is_replaced() {
        // Upgrading jabar must upgrade the aspect, or a new server runs an old
        // one and the mismatch is silent.
        let dir = tempfile::tempdir().expect("temp dir");
        let root = root(&dir);
        let runner = AspectRunner::new(&root, "bazel", None);
        runner.install().expect("install");

        let aspect = dir.path().join(".jabar/aspects/scip_java.bzl");
        std::fs::write(&aspect, "# an older version").expect("write");
        assert!(runner.install().expect("install"), "difference is rewritten");
        assert!(std::fs::read_to_string(&aspect).unwrap().contains("scip_java_aspect"));
    }

    #[test]
    fn a_never_built_workspace_is_refused_with_a_reason() {
        // Bazel's own error here is "output '...scip' was not created", which
        // says nothing about the cause. This one does.
        let dir = tempfile::tempdir().expect("temp dir");
        let root = root(&dir);
        let runner = AspectRunner::new(&root, "bazel", None);
        let config = AspectConfig {
            targets: vec!["//...".to_owned()],
            scip_java: Utf8PathBuf::from("/usr/local/bin/scip-java"),
            java_home: Utf8PathBuf::from("/usr/lib/jvm"),
        };
        let err = runner.run(&config).expect_err("no bazel-bin symlink");
        assert!(matches!(err, AspectError::NotBuilt));
        assert!(err.to_string().contains("bazel build"), "names the fix: {err}");
    }
}
