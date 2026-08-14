# What this fixture is for

A deliberately small slice of a Java megarepo — 12 Bazel targets, 28 source
files, 844 lines — shaped so that every query jabar has to answer has exactly
one right answer that is cheap to state and easy to check.

It is not a sample application. Every structural choice below exists to make
some specific failure mode visible.

Build and run it:

```
bazel build //...
bazel run //java/com/acme/app:app
```

## The target graph

```
                        third_party/tinyjson  (java_import — classfiles only)
                                 │
  core ────┬──── backoff ──── policy ──── transport ──┬── service/order
   ▲       │        ▲            ▲           ▲        ├── service/inventory
   │       │        │            │           │        └── service/payment
  util ────┴────────┴────────────┴───────────┘                  │
   ▲                                                            │
   └──── i18n            generated            orphan/ (no BUILD) │
                                                                │
                     app  ◄──────────────────────────────────────┘
```

`core` and `util` sit at the bottom with no dependencies. `util` has the
highest fan-in: eight of the eleven other targets depend on it.

## Slice boundaries

The focus-slice claim is only meaningful if a slice is measurably smaller than
the repo. Rooted at `//java/com/acme/service/order`:

| In the transitive closure | Outside it |
| --- | --- |
| `service/order`, `transport`, `policy`, `backoff`, `core`, `util`, `third_party/tinyjson` | `app`, `generated`, `i18n`, `service/inventory`, `service/payment` |

Seven targets in, five out. `service/inventory` is deliberately *not* dependent
on `policy`, so a slice rooted there is strictly smaller again — which is the
check that slicing is real rather than "load everything and call it a slice."

## Per-operation expectations

Line numbers are omitted on purpose; they drift as the fixture is edited. Derive
them and assert on the target/file sets, which are stable.

| Operation | Query | Expected answer |
| --- | --- | --- |
| `workspaceSymbol` | `RetryPolicy` | 1 interface in `core`, 3 implementing classes in `policy` (×2) and `service/payment` (×1) |
| `workspaceSymbol` | `Backoff` | 1 interface in `core`, 3 impls all in `backoff` |
| `workspaceSymbol` | `Json` | `com.tinyjson.Json` — from the jar, with no source file anywhere |
| `goToDefinition` | `Json.parse` in `transport/Response.java` | Resolves into `tinyjson.jar`, not a `.java` file |
| `goToDefinition` | `String.strip` in `util/Strings.java` | Resolves into the JDK — which in Java 9+ is `lib/modules`, a jimage archive, not a jar |
| `goToImplementation` | `RetryPolicy` | `DefaultRetryPolicy`, `CircuitBreakerPolicy`, `PaymentRetryPolicy` — spanning 2 targets, neither of them `core` |
| `goToImplementation` | `Backoff` | `ExponentialBackoff`, `FixedBackoff`, `JitteredBackoff` — all in 1 target |
| `goToImplementation` | `HttpClient` | `SimpleHttpClient`, `RetryingHttpClient` |
| `findReferences` | `Preconditions.checkNotNull` | 30 call sites across 15 files in 8 targets, plus 1 declaration and 1 `{@link}` in javadoc. **The truncation case.** |
| `findReferences` | `Clock.nowMillis` | 3 sites, 2 targets — small enough to return whole |
| `documentSymbol` | `policy/PolicyRegistry.java` | 1 class, 1 field, 3 methods. No graph access needed. |
| `outgoingCalls` | `OrderService.placeOrder` | Crosses into `transport`, then `policy`, then `backoff` — 3 target boundaries |
| `incomingCalls` | `Backoff.nextDelay` | `RetryingHttpClient.send` in `transport`, reached only via reverse-dependency edges |
| `hover` | `RetryingHttpClient` | Javadoc plus resolved supertypes; local to one slice |

### Why `checkNotNull` is the important one

30 call sites is small enough to verify by hand and large enough that ranking
has to do something. The correct behaviour is not "return 30 locations": it is
to rank by proximity to the query site, group by target, state the true total,
and attach a line of surrounding context per hit. In the real repo this symbol
has tens of thousands of call sites, and a client that receives an unranked
truncated list is confidently wrong about where its symbol is used.

It also carries a second, smaller test. Beyond the 30 calls there is one
declaration and one `{@link #checkNotNull}` in javadoc. Those are three
different kinds of reference, and collapsing them into one list loses
information the caller needs — a rename touches all three, a call-graph query
wants only the first.

## Edge cases and what each one proves

**`java/com/acme/i18n/Messages.java` — position encoding.**
Every offset in this file differs across UTF-8 bytes, UTF-16 code units and
codepoints. Line 21 is the sharp case: 63 bytes, 59 UTF-16 code units, 58
codepoints, because `🔁` is outside the BMP and costs 4 bytes / 2 code units / 1
codepoint. LSP defaults to UTF-16; jabar's internal offsets are bytes. Any range
returned from this file is a live test of the conversion. The file also declares
a method named `grüße`, so symbol indexing has to survive non-ASCII identifiers
— Error Prone rejects those by default, and the check is disabled for this
target specifically to keep the test.

**`java/com/acme/orphan/NotInAnyTarget.java` — no owning target.**
There is no `BUILD.bazel` in that directory. `buildTarget/inverseSources`
returns nothing. The required behaviour is to index it standalone with an empty
classpath, not to fail the request. Real repos are full of these.

**`java/com/acme/generated/BuildVersion.java` — generated source.**
Exists only under `bazel-out`. A filesystem walk of the source tree never finds
it; it is reachable only through the build graph. `app` depends on it, so
`BuildVersion.describe` must resolve.

**`third_party/tinyjson/tinyjson.jar` — binary-only dependency.**
Classfiles with no sources on the classpath, consumed via `java_import`. This is
how the bulk of a real Java classpath arrives, and it is the case rust-analyzer
has no analogue for. Everything jabar knows about `com.tinyjson` has to come
from reading the jar. Reference sources live in `_src_reference/` and are in no
Bazel target; regenerate with `tools/build_thirdparty_jar.sh`.

**JDK types — the other binary dependency.**
`String`, `List`, `Optional`, `Duration` are used throughout. Since Java 9 these
do not live in a jar at all; they are in `$JAVA_HOME/lib/modules`, a jimage
archive. A classfile reader that only knows how to open zip archives resolves
nothing from the JDK, which is most of what a Java file references.

## Debugging

`//java/com/acme/app:app` is runnable, so the fixture can be stepped through
rather than only indexed:

```
bazel run //java/com/acme/app:app
```

Attach a debugger by running the binary directly with JDWP:

```
bazel build //java/com/acme/app:app
bazel-bin/java/com/acme/app/app --jvm_flag=-agentlib:jdwp=transport=dt_socket,server=y,suspend=y,address=5005
```

Note that debugging is DAP, not LSP — a separate protocol with a separate
server. jabar's role there is supplying source-to-class mapping and breakpoint
resolution, not answering debug requests itself.

## Known gaps

An external review identified three cases this fixture cannot exercise, each of
which hides a failure that only appears at real scale. Adding them is worthwhile
before M4:

- **No target large enough to trigger a params file.** Bazel moves a javac
  command line into an `@file` once it crosses a size threshold, at which point
  `build-model`'s argv scan finds no flags and silently produces an empty
  `CompileInfo`. Forcing this with `--min_param_file_size=1` did not reproduce it
  here, so a target with a genuinely large classpath is needed. See "A known
  break waiting at scale" in `docs/phase-1.md`.
- **No `java_proto_library`.** Generated protobuf code dominates real megarepo
  classpaths and carries its own jar-naming conventions.
- **No annotation processors.** They generate sources at build time, which is a
  different shape of generated code from the single `genrule` here.

## Extending the fixture

Keep the invariants that make it useful:

- Every target's dependency set stays explicit and minimal, so slice boundaries
  stay checkable.
- New symbols get either very high or very low fan-in — the middle teaches
  nothing.
- Anything added to `third_party/` arrives as a binary, never as source.
- If a file is added to prove an edge case, say which one in its class Javadoc.
