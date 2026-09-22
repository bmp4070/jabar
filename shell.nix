# Development shell for jabar.
#
# Provides the exact Rust 1.97.1 toolchain pinned in rust-toolchain.toml
# (rustc, cargo, clippy, rustfmt, rust-analyzer) plus the JVM tooling the
# monolith benchmark harness needs: a JDK and coursier, from which scip-java
# is bootstrapped on first entry.
#
#   nix-shell            # from the repo root
#
# nixpkgs is pinned by revision, so every developer gets byte-identical tools
# rather than whatever their channel happens to point at.
{ }:
let
  # nixpkgs-unstable @ 174eb78 — the revision that ships Rust 1.97.1, matching
  # rust-toolchain.toml. When bumping the Rust channel, bump this in lockstep
  # (find a rev whose `rustc.version` matches, refresh the sha256).
  nixpkgs = fetchTarball {
    url = "https://github.com/NixOS/nixpkgs/archive/174eb786fb68e3a13e4e535a3deea479a0c07a6a.tar.gz";
    sha256 = "01b6yi3rn1hlr9wz5lvb7nsyf6gjmhvvb0nmmlxaqag107jcwqkm";
  };
  pkgs = import nixpkgs { };

  # scip-java is not packaged in nixpkgs, so we bootstrap it via coursier on
  # first shell entry. Pin the version here.
  scipJavaVersion = "0.12.3";
in
pkgs.mkShell {
  name = "jabar-dev";

  packages = [
    # Rust toolchain — matches rust-toolchain.toml (channel = "1.97.1").
    # These are real binaries from nix; they take precedence over any rustup
    # shims on PATH, so rust-toolchain.toml never triggers a network fetch.
    pkgs.rustc
    pkgs.cargo
    pkgs.clippy
    pkgs.rustfmt
    pkgs.rust-analyzer

    # JVM tooling for the benchmark harness: the scip-java aspect / auto-indexing.
    pkgs.jdk21
    pkgs.coursier
  ];

  shellHook = ''
    export JAVA_HOME="${pkgs.jdk21.home}"

    # scip-java is cached under the repo (gitignored) so it survives shell exits
    # but is not committed. Bootstrapped once via coursier; needs network the
    # first time (works through the SFDC nexus proxy).
    scip_java_dir="$PWD/.cache/scip-java/${scipJavaVersion}"
    export PATH="$scip_java_dir/bin:$PATH"
    if [ ! -x "$scip_java_dir/bin/scip-java" ]; then
      echo "jabar-dev: bootstrapping scip-java ${scipJavaVersion} (first run)…" >&2
      mkdir -p "$scip_java_dir/bin"
      cs bootstrap "com.sourcegraph:scip-java_2.13:${scipJavaVersion}" \
        -M com.sourcegraph.scip_java.ScipJava \
        -o "$scip_java_dir/bin/scip-java" --standalone \
        || echo "jabar-dev: scip-java bootstrap failed (offline?); auto-indexing unavailable" >&2
    fi

    echo "jabar-dev: rustc $(rustc --version | cut -d' ' -f2) · scip-java ${scipJavaVersion} on PATH" >&2
  '';
}
