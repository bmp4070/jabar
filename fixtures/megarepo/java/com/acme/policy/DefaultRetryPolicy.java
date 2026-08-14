package com.acme.policy;

import com.acme.backoff.ExponentialBackoff;
import com.acme.core.Backoff;
import com.acme.core.RetryPolicy;
import com.acme.util.Preconditions;
import java.io.IOException;

/** Retries anything that looks transient, up to a fixed attempt count. */
public final class DefaultRetryPolicy implements RetryPolicy {

  private final int maxAttempts;
  private final Backoff backoff;

  public DefaultRetryPolicy(int maxAttempts, Backoff backoff) {
    this.maxAttempts = Preconditions.checkPositive(maxAttempts, "maxAttempts");
    this.backoff = Preconditions.checkNotNull(backoff, "backoff");
  }

  public static DefaultRetryPolicy defaults() {
    return new DefaultRetryPolicy(3, ExponentialBackoff.defaults());
  }

  @Override
  public String name() {
    return "default";
  }

  @Override
  public boolean shouldRetry(int attempt, Throwable failure) {
    if (attempt >= maxAttempts) {
      return false;
    }
    return failure instanceof IOException;
  }

  @Override
  public int maxAttempts() {
    return maxAttempts;
  }

  public Backoff backoff() {
    return backoff;
  }
}
