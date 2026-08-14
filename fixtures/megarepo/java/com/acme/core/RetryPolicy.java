package com.acme.core;

/**
 * Decides whether a failed attempt is worth repeating.
 *
 * <p>Implemented in three different Bazel targets on purpose: {@code
 * //java/com/acme/policy} carries two implementations and {@code
 * //java/com/acme/service/payment} carries a third. A goToImplementation query
 * from here has to leave the current target to find all of them.
 */
public interface RetryPolicy {

  /** Human-readable name, used in logs and by {@code PolicyRegistry}. */
  String name();

  /** Returns true when attempt number {@code attempt} should be retried. */
  boolean shouldRetry(int attempt, Throwable failure);

  /** Upper bound on attempts, inclusive of the first one. */
  int maxAttempts();
}
