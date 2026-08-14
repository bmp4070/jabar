# The SCIP indexing aspect

`scip_java.bzl` is a fork of `sourcegraph/scip-java` v0.12.3's aspect, carrying
six fixes for Bazel 9. The JVM indexer it invokes is used unmodified — only the
Starlark is ours. Each change is marked `JABAR:` in the file.

## Running it

The aspect writes its outputs relative to `sourceroot`, and expects
`sourceroot/bazel-out` to already be the convenience symlink into the execroot.
On a workspace that has never been built, that symlink does not exist, the
aspect creates a real directory in its place, and every output lands somewhere
Bazel will not look — reported as `output '...scip' was not created`, which does
not name the real cause.

**So a plain build has to happen first.** This is not a workaround; it is the
same prerequisite F17 arrives at from the other direction, since an index is a
build output either way.

```sh
export JAVA_HOME=$(/usr/libexec/java_home)

bazel build //...                              # must precede the aspect

bazel build //... \
  --aspects //aspects:scip_java.bzl%scip_java_aspect \
  --output_groups=scip \
  --define=sourceroot=$PWD \
  --define=java_home=$JAVA_HOME \
  --define=scip_java_binary=$(which scip-java)
```

One `.scip` shard is produced per Java target under `bazel-bin/`. Shards
concatenate: `find bazel-bin -name '*.scip' | xargs cat > index.scip` is a valid
combined index.

## Installing it

The aspect must live inside the workspace being indexed, because Bazel loads it
by label. Copy this file to `<workspace>/aspects/scip_java.bzl` alongside a
`BUILD.bazel` containing `exports_files(["scip_java.bzl"])`. jabar will do this
itself; until then it is manual.

Do not use `scip-java index` to install it — that command rewrites the file with
the upstream version, undoing the fixes, and in any case does not detect
bzlmod-only workspaces.

## Verified against

Bazel 9.2.0, JDK 26, scip-java 0.12.3, macOS arm64, on `fixtures/megarepo`.
Reproduces `EXPECTATIONS.md` exactly: 30 `checkNotNull` references across 15
files in 8 targets; 3 `RetryPolicy`, 3 `Backoff` and 2 `HttpClient`
implementations. Also indexes the non-ASCII `grüße` identifier and the
genrule-produced `BuildVersion.java`, which exists only under `bazel-out`.

## Upstreaming

Fixes 2–6 are general Bazel 9 compatibility and belong upstream. Fix 1 (bzlmod
detection) lives in the JVM side of `scip-java index`, not here.
