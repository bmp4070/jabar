package com.acme.backoff;

import com.acme.core.Backoff;
import com.acme.util.Preconditions;
import java.time.Duration;

/** Waits the same amount every time. */
public final class FixedBackoff implements Backoff {

  private final Duration delay;

  public FixedBackoff(Duration delay) {
    this.delay = Preconditions.checkNotNull(delay, "delay");
  }

  @Override
  public Duration nextDelay(int attempt) {
    Preconditions.checkPositive(attempt, "attempt");
    return delay;
  }

  @Override
  public String describe() {
    return "fixed(" + delay.toMillis() + "ms)";
  }
}
