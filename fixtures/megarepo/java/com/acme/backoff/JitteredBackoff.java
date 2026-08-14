package com.acme.backoff;

import com.acme.core.Backoff;
import com.acme.util.Preconditions;
import java.time.Duration;
import java.util.random.RandomGenerator;

/** Wraps another {@link Backoff} and spreads the delay to avoid thundering herds. */
public final class JitteredBackoff implements Backoff {

  private final Backoff delegate;
  private final RandomGenerator random;

  public JitteredBackoff(Backoff delegate, RandomGenerator random) {
    this.delegate = Preconditions.checkNotNull(delegate, "delegate");
    this.random = Preconditions.checkNotNull(random, "random");
  }

  @Override
  public Duration nextDelay(int attempt) {
    Duration base = delegate.nextDelay(attempt);
    long jittered = (long) (base.toMillis() * (0.5 + random.nextDouble() * 0.5));
    return Duration.ofMillis(jittered);
  }

  @Override
  public String describe() {
    return "jittered(" + delegate.describe() + ")";
  }
}
