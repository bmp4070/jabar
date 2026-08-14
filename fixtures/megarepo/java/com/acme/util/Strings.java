package com.acme.util;

import java.util.List;

/** Small string helpers, deliberately depending only on the JDK. */
public final class Strings {

  private Strings() {}

  public static boolean isBlank(String value) {
    return value == null || value.strip().isEmpty();
  }

  public static String orDefault(String value, String fallback) {
    return isBlank(value) ? fallback : value;
  }

  /** Joins with ", " — used by several services when logging. */
  public static String commaJoin(List<String> parts) {
    Preconditions.checkNotNull(parts, "parts");
    return String.join(", ", parts);
  }
}
