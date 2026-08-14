package com.acme.orphan;

/**
 * Compiles fine, but no BUILD file references it.
 *
 * <p>There is no BUILD.bazel in this directory on purpose. An inverseSources
 * query for this path returns nothing, and jabar has to degrade to indexing it
 * standalone with an empty classpath rather than failing the request. Real
 * repos are full of these: scratch files, half-deleted code, and sources that
 * only some downstream genrule consumes.
 */
public final class NotInAnyTarget {

  private NotInAnyTarget() {}

  public static String orphaned() {
    return "not reachable from any bazel target";
  }
}
