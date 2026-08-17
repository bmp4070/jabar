//! What the client can tell jabar at startup.
//!
//! Arrives as LSP `initializationOptions`, which every client can set — VS Code
//! through its extension, Claude Code through its plugin manifest. Anything
//! absent takes a default that works without configuration, because a server
//! that needs a config file before it does anything is a server most people
//! never see working.

use paths::Utf8PathBuf;
use serde::Deserialize;

/// Client-supplied settings.
#[derive(Debug, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Config {
    /// Where bazel should keep its state.
    ///
    /// Absent means share the workspace default, which is the right choice when
    /// nothing else is building: same server, same action cache, no duplicate
    /// analysis. Set it when jabar's builds would otherwise queue behind — and
    /// block — the user's own, since bazel takes an exclusive lock per output
    /// base.
    ///
    /// The cost of a separate base is a second analysis universe and a second
    /// set of outputs, which on a large repo is gigabytes and a cold analysis
    /// the first time.
    pub output_base: Option<Utf8PathBuf>,

    /// The `bazel` executable. `bazelisk` also works.
    pub bazel: Option<String>,

    /// Indexing behaviour.
    pub index: IndexConfig,
}

#[derive(Debug, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct IndexConfig {
    /// Run the aspect at startup when no index is found.
    ///
    /// Off by default. Indexing runs a Bazel build, which can take minutes and
    /// which the user did not ask for by opening an editor; doing that
    /// unbidden is a poor first impression and, on a shared output base, will
    /// contend with whatever else they are running.
    pub auto: bool,

    /// What to index.
    ///
    /// Deliberately not `//...`: on a real megarepo that includes targets
    /// broken at HEAD, targets needing credentials, and targets whose
    /// toolchains are not installed. Scoping is the normal case.
    pub targets: Vec<String>,

    /// Path to `scip-java`. Absent means look it up on `PATH`.
    pub scip_java: Option<Utf8PathBuf>,
}

impl Default for IndexConfig {
    fn default() -> IndexConfig {
        IndexConfig { auto: false, targets: vec!["//...".to_owned()], scip_java: None }
    }
}

impl Config {
    /// Reads `initializationOptions`, falling back to defaults on anything
    /// unparseable.
    ///
    /// A malformed setting should not stop the server starting: the client can
    /// still use it, and a warning in the log beats a failed handshake with no
    /// features and no explanation.
    pub fn from_initialization_options(options: Option<&serde_json::Value>) -> Config {
        let Some(options) = options else { return Config::default() };
        match serde_json::from_value(options.clone()) {
            Ok(config) => config,
            Err(err) => {
                tracing::warn!(%err, "ignoring malformed initializationOptions");
                Config::default()
            }
        }
    }

    /// The `scip-java` binary to use, looked up on `PATH` when unset.
    pub fn resolve_scip_java(&self) -> Option<Utf8PathBuf> {
        if let Some(configured) = &self.scip_java_path() {
            return Some((*configured).clone());
        }
        let path = std::env::var_os("PATH")?;
        std::env::split_paths(&path)
            .map(|dir| dir.join("scip-java"))
            .find(|candidate| candidate.is_file())
            .and_then(|found| Utf8PathBuf::from_path_buf(found).ok())
    }

    fn scip_java_path(&self) -> Option<&Utf8PathBuf> {
        self.index.scip_java.as_ref()
    }
}

/// `JAVA_HOME`, which `scip-java` requires and does not infer.
pub fn java_home() -> Option<Utf8PathBuf> {
    let raw = std::env::var_os("JAVA_HOME")?;
    Utf8PathBuf::from_path_buf(raw.into()).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn no_options_means_working_defaults() {
        let config = Config::from_initialization_options(None);
        assert_eq!(config.output_base, None, "share the workspace base");
        assert_eq!(config.bazel, None);
        assert!(!config.index.auto, "never build unbidden");
        assert_eq!(config.index.targets, ["//..."]);
    }

    #[test]
    fn settings_are_read_from_initialization_options() {
        let options = json!({
            "outputBase": "/tmp/jabar-base",
            "bazel": "bazelisk",
            "index": { "auto": true, "targets": ["//java/...", "//lib/..."] }
        });
        let config = Config::from_initialization_options(Some(&options));
        assert_eq!(config.output_base.as_deref().map(|p| p.as_str()), Some("/tmp/jabar-base"));
        assert_eq!(config.bazel.as_deref(), Some("bazelisk"));
        assert!(config.index.auto);
        assert_eq!(config.index.targets, ["//java/...", "//lib/..."]);
    }

    #[test]
    fn a_partial_config_keeps_the_other_defaults() {
        let config =
            Config::from_initialization_options(Some(&json!({ "index": { "auto": true } })));
        assert!(config.index.auto);
        assert_eq!(config.index.targets, ["//..."], "untouched by the override");
        assert_eq!(config.output_base, None);
    }

    #[test]
    fn malformed_options_do_not_stop_the_server() {
        // A bad setting should cost the setting, not the session.
        let config =
            Config::from_initialization_options(Some(&json!({ "index": "not an object" })));
        assert_eq!(config.index.targets, ["//..."]);
        assert!(!config.index.auto);

        // Including a wholly unexpected shape.
        let config = Config::from_initialization_options(Some(&json!(["array", "of", "things"])));
        assert_eq!(config.index.targets, ["//..."]);
    }

    #[test]
    fn unknown_settings_are_ignored_rather_than_fatal() {
        // A newer client sending a setting this build does not know about
        // should not lose the settings it does know.
        let options = json!({ "outputBase": "/tmp/b", "somethingFromTheFuture": 42 });
        let config = Config::from_initialization_options(Some(&options));
        assert_eq!(config.output_base.as_deref().map(|p| p.as_str()), Some("/tmp/b"));
    }
}
