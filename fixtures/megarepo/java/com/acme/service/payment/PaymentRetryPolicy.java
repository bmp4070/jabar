package com.acme.service.payment;

import com.acme.core.RetryPolicy;
import com.acme.util.Preconditions;
import java.io.IOException;

/**
 * Third implementation of {@link RetryPolicy}, and the one furthest from it.
 *
 * <p>Payments are not idempotent, so this refuses to retry anything that might
 * have been applied server-side.
 */
public final class PaymentRetryPolicy implements RetryPolicy {

  private final int maxAttempts;

  public PaymentRetryPolicy(int maxAttempts) {
    this.maxAttempts = Preconditions.checkPositive(maxAttempts, "maxAttempts");
  }

  @Override
  public String name() {
    return "payment";
  }

  @Override
  public boolean shouldRetry(int attempt, Throwable failure) {
    if (attempt >= maxAttempts) {
      return false;
    }
    // Only retry when we are certain the request never reached the processor.
    return failure instanceof IOException io && io.getMessage() != null
        && io.getMessage().contains("connect");
  }

  @Override
  public int maxAttempts() {
    return maxAttempts;
  }
}
