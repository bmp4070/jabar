"""
Bazel aspect emitting SCIP indexes, forked for Bazel 9.

Upstream: sourcegraph/scip-java v0.12.3, `aspects/scip_java.bzl`, which
`scip-java index` writes into the workspace. That version targets an older
Bazel and fails six ways on 9.2.0. This fork carries the fixes; the JVM
indexer it invokes is used unmodified.

Every change is marked `JABAR:` below. In order of discovery:

1. Detection. `scip-java index` looks for a `WORKSPACE` file, so a bzlmod-only
   repo is not recognised as Bazel at all, even with `--build-tool bazel`. Not
   fixable here -- jabar invokes `bazel build --aspects` directly rather than
   going through `scip-java index`.
2. `JavaInfo` is no longer a Starlark global; it moved into `rules_java`.
3. An aspect implementation may no longer return a `struct`.
4. `JavaCompilationInfo.javac_options` is a `depset`, not a list.
5. `struct.to_json()` was removed in favour of the `json` module.
6. Those javac options arrive as a single shell-quoted string rather than one
   option per element, and javac rejects the concatenation. See
   `_split_shell_words`.

These are worth upstreaming; until they are, this file is the source of truth
and is written into the workspace under test.
"""

load("@rules_java//java/common:java_info.bzl", "JavaInfo")  # JABAR: fix 2


# JABAR: fix 6.
def _split_shell_words(text):
    """Splits a shell-quoted argument string into individual arguments.

    Bazel 9 hands `javac_options` back as one string rather than a list:

        -source 21 '-XDcompilePolicy=simple' -Xep:ReturnMissingNullable:OFF

    Splitting on spaces alone would be right here but wrong in general, since
    an option's value may contain one -- `-Xbootclasspath/p:/a b/c.jar`, or any
    path with a space. Single quotes are what Bazel emits, so they are what is
    honoured; a quote toggles whether a space separates.
    """
    args = []
    current = ""
    started = False
    quoted = False
    for ch in text.elems():
        if ch == "'":
            quoted = not quoted
            started = True
        elif ch == " " and not quoted:
            if started:
                args.append(current)
            current = ""
            started = False
        else:
            current += ch
            started = True
    if started:
        args.append(current)
    return args

def _scip_java(target, ctx):
    if JavaInfo not in target or not hasattr(ctx.rule.attr, "srcs"):
        return None

    javac_action = None
    for a in target.actions:
        if a.mnemonic == "Javac":
            javac_action = a
            break

    if not javac_action:
        return None

    info = target[JavaInfo]
    compilation = info.compilation_info
    annotations = info.annotation_processing

    source_files = []
    source_jars = []
    for src in ctx.rule.files.srcs:
        if src.path.endswith(".java"):
            source_files.append(src.path)
        elif src.path.endswith(".srcjar"):
            source_jars.append(src)

    if len(source_files) == 0:
        return None

    output_dir = []

    for source_jar in source_jars:
        dir = ctx.actions.declare_directory(ctx.label.name + ".extracted_srcjar/" + source_jar.short_path)
        output_dir.append(dir)

        ctx.actions.run_shell(
            inputs = javac_action.inputs,
            outputs = [dir],
            mnemonic = "ExtractSourceJars",
            command = """
                [ "$(unzip -q -l {input_file} | wc -l)" -eq 0 ] || unzip {input_file} -d {output_dir}
            """.format(
                output_dir = dir.path,
                input_file = source_jar.path,
            ),
            progress_message = "Extracting source jar {jar}".format(jar = source_jar.path),
        )

        source_files.append(dir.path)

    classpath = [j.path for j in compilation.compilation_classpath.to_list()]
    bootclasspath = [j.path for j in compilation.boot_classpath]

    processorpath = []
    processors = []
    if annotations and annotations.enabled:
        processorpath += [j.path for j in annotations.processor_classpath.to_list()]
        processors = annotations.processor_classnames

    launcher_javac_flags = []
    compiler_javac_flags = []

    # In different versions of bazel javac options are either a nested set or a depset or a list...
    javac_options = []
    if hasattr(compilation, "javac_options_list"):
        javac_options = compilation.javac_options_list
    elif type(compilation.javac_options) == "depset":
        # JABAR: fixes 4 and 6. Bazel 9 returns a depset, and its elements are
        # not individual options -- one element holds the whole option string,
        # shell-quoted. Flatten, then split each element respecting quotes.
        for chunk in compilation.javac_options.to_list():
            javac_options += _split_shell_words(chunk)
    else:
        javac_options = compilation.javac_options

    for value in javac_options:
        # NOTE(Anton): for some bizarre reason I see empty string starting the list of
        # javac options - which then gets propagated into the JSON config, and ends up
        # crashing the actual javac invokation.
        if value != "":
            if value.startswith("-J"):
                launcher_javac_flags.append(value)
            else:
                compiler_javac_flags.append(value)

    build_config = struct(**{
        "javaHome": ctx.var["java_home"],
        "classpath": classpath,
        "sourceFiles": source_files,
        "javacOptions": compiler_javac_flags,
        "jvmOptions": launcher_javac_flags,
        "processors": processors,
        "processorpath": processorpath,
        "bootclasspath": bootclasspath,
        "reportWarningOnEmptyIndex": False,
    })
    build_config_path = ctx.actions.declare_file(ctx.label.name + ".scip.json")

    scip_output = ctx.actions.declare_file(ctx.label.name + ".scip")
    targetroot = ctx.actions.declare_directory(ctx.label.name + ".semanticdb")
    ctx.actions.write(
        output = build_config_path,
        content = json.encode(build_config),  # JABAR: fix 5
    )

    deps = [javac_action.inputs, annotations.processor_classpath]

    ctx.actions.run_shell(
        command = "\"{}\" index --no-cleanup --index-semanticdb.allow-empty-index --cwd \"{}\" --targetroot {} --scip-config \"{}\" --output \"{}\"".format(
            ctx.var["scip_java_binary"],
            ctx.var["sourceroot"],
            targetroot.path,
            build_config_path.path,
            scip_output.path,
        ),
        env = {
            "JAVA_HOME": ctx.var["java_home"],
            "NO_PROGRESS_BAR": "true",
        },
        mnemonic = "ScipJavaIndex",
        inputs = depset([build_config_path] + output_dir, transitive = deps),
        outputs = [scip_output, targetroot],
    )

    return scip_output

def _scip_java_aspect(target, ctx):
    scip = _scip_java(target, ctx)
    if not scip:
        return []  # JABAR: fix 3 -- a struct return is rejected on Bazel 9
    return [OutputGroupInfo(scip = [scip])]

scip_java_aspect = aspect(
    _scip_java_aspect,
)

def _scip_java_impl(ctx):
    output = ctx.attr.compilation[OutputGroupInfo]
    return [
        OutputGroupInfo(scip = output.scip),
        DefaultInfo(files = output.scip),
    ]

scip_java = rule(
    implementation = _scip_java_impl,
    attrs = {"compilation": attr.label(aspects = [scip_java_aspect])},
)
