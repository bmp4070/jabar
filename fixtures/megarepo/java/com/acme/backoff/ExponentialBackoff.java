package com.acme.backoff;

import com.acme.core.Backoff;
import com.acme.util.Preconditions;
import java.time.Duration;

/** Doubles the delay on each attempt, up to a ceiling. */
public final class ExponentialBackoff implements Backoff {

  private final Duration base;
  private final Duration ceiling;

  public ExponentialBackoff(Duration base, Duration ceiling) {
    this.base = Preconditions.checkNotNull(base, "base");
    this.ceiling = Preconditions.checkNotNull(ceiling, "ceiling");
  }

  public static ExponentialBackoff defaults() {
    return new ExponentialBackoff(Duration.ofMillis(100), Duration.ofSeconds(30));
  }

  @Override
  public Duration nextDelay(int attempt) {
    Preconditions.checkPositive(attempt, "attempt");
    long millis = base.toMillis() << Math.min(attempt - 1, 20);
    return Duration.ofMillis(Math.min(millis, ceiling.toMillis()));
  }

  @Override
  public String describe() {
    return "exponential(" + base.toMillis() + "ms.." + ceiling.toMillis() + "ms)";
  }
}
