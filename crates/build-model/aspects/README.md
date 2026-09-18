# The SCIP indexing aspect

Jabar bundles an unmodified copy of the [upstream `scip-java` Bazel aspect](https://github.com/scip-code/scip-java/blob/main/scip-java/src/main/resources/scip-java/scip_java.bzl),
at commit `0e47f47c4aebf47ce7f739eb51fa50938f3356d5` (2026-09-16).
Upstream now handles Bazel 9, bzlmod-only workspaces, and shell-quoted javac
options. A separate `bmp4070/scip-java` fork is unnecessary. The bundled file
exists because Bazel loads aspects by a label inside the workspace; Jabar writes
it to `<workspace>/.jabar/aspects/` when indexing.

Use a `scip-java` binary with the matching upstream CLI. The old v0.12.3
binary and this aspect are not a tested pair.

## Running it

Jabar installs the aspect and runs Bazel when `index.auto` is enabled. To run it
manually in a Bazel workspace, copy `scip_java.bzl` to `aspects/`, add a
`BUILD.bazel` there with `exports_files(["scip_java.bzl"])`, then run:

```sh
export JAVA_HOME=$(/usr/libexec/java_home)

bazel build //... \
  --aspects //aspects:scip_java.bzl%scip_java_aspect \
  --output_groups=scip \
  --define=java_home=$JAVA_HOME \
  --define=scip_java_binary=$(which scip-java)
```

The aspect writes a `<target>.scip` shard for each indexed Java target under
`bazel-bin/`. The sibling `<target>.scip-targetroot/` contains intermediate
per-source shards; exclude those when loading or concatenating an index:

```sh
find bazel-bin -type f -name '*.scip' -not -path '*.scip-targetroot/*' | xargs cat > index.scip
```

The earlier v0.12.3 aspect was verified on Bazel 9.2.0, JDK 26, and the
`fixtures/megarepo` workspace. The current upstream snapshot has not yet been
run against that fixture in this repository.
