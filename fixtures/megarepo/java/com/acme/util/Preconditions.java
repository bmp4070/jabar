package com.acme.util;

/**
 * Argument checks.
 *
 * <p>{@link #checkNotNull} is called from most files in this repo. It is the
 * fixture's high-fan-out reference target: a findReferences here must rank and
 * truncate rather than return everything.
 */
public final class Preconditions {

  private Preconditions() {}

  public static <T> T checkNotNull(T value, String what) {
    if (value == null) {
      throw new IllegalArgumentException(what + " must not be null");
    }
    return value;
  }

  public static int checkPositive(int value, String what) {
    if (value <= 0) {
      throw new IllegalArgumentException(what + " must be positive, got " + value);
    }
    return value;
  }

  public static void checkState(boolean condition, String message) {
    if (!condition) {
      throw new IllegalStateException(message);
    }
  }
}
