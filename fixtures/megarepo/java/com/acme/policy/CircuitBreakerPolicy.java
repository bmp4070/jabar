package com.acme.policy;

import com.acme.core.Clock;
import com.acme.core.RetryPolicy;
import com.acme.core.SystemClock;
import com.acme.util.Preconditions;

/** Stops retrying entirely once failures cluster inside a time window. */
public final class CircuitBreakerPolicy implements RetryPolicy {

  private final RetryPolicy delegate;
  private final Clock clock;
  private final int threshold;

  private int consecutiveFailures;
  private long openedAtMillis;

  public CircuitBreakerPolicy(RetryPolicy delegate, int threshold) {
    this(delegate, threshold, SystemClock.instance());
  }

  public CircuitBreakerPolicy(RetryPolicy delegate, int threshold, Clock clock) {
    this.delegate = Preconditions.checkNotNull(delegate, "delegate");
    this.clock = Preconditions.checkNotNull(clock, "clock");
    this.threshold = Preconditions.checkPositive(threshold, "threshold");
  }

  @Override
  public String name() {
    return "circuit-breaker(" + delegate.name() + ")";
  }

  @Override
  public boolean shouldRetry(int attempt, Throwable failure) {
    if (isOpen()) {
      return false;
    }
    boolean retry = delegate.shouldRetry(attempt, failure);
    if (!retry) {
      recordFailure();
    }
    return retry;
  }

  @Override
  public int maxAttempts() {
    return delegate.maxAttempts();
  }

  private boolean isOpen() {
    return consecutiveFailures >= threshold && clock.nowMillis() - openedAtMillis < 30_000L;
  }

  private void recordFailure() {
    consecutiveFailures++;
    if (consecutiveFailures == threshold) {
      openedAtMillis = clock.nowMillis();
    }
  }
}
