package com.acme.transport;

import com.acme.core.Backoff;
import com.acme.core.RetryException;
import com.acme.core.RetryPolicy;
import com.acme.policy.DefaultRetryPolicy;
import com.acme.util.Preconditions;
import java.io.IOException;
import java.time.Duration;

/**
 * Wraps another {@link HttpClient} with a {@link RetryPolicy}.
 *
 * <p>This sits in the middle of the fixture's cross-target call chain:
 * {@code OrderService.placeOrder} calls {@link #send}, which calls
 * {@code RetryPolicy.shouldRetry} and {@code Backoff.nextDelay}. Those four
 * frames live in four different Bazel targets.
 */
public final class RetryingHttpClient implements HttpClient {

  private final HttpClient delegate;
  private final RetryPolicy policy;
  private final Backoff backoff;

  public RetryingHttpClient(HttpClient delegate, RetryPolicy policy, Backoff backoff) {
    this.delegate = Preconditions.checkNotNull(delegate, "delegate");
    this.policy = Preconditions.checkNotNull(policy, "policy");
    this.backoff = Preconditions.checkNotNull(backoff, "backoff");
  }

  public static RetryingHttpClient withDefaults(HttpClient delegate) {
    DefaultRetryPolicy policy = DefaultRetryPolicy.defaults();
    return new RetryingHttpClient(delegate, policy, policy.backoff());
  }

  @Override
  public Response send(Request request) throws IOException {
    Preconditions.checkNotNull(request, "request");
    IOException last = null;
    for (int attempt = 1; attempt <= policy.maxAttempts(); attempt++) {
      try {
        Response response = delegate.send(request);
        if (!response.isRetryable()) {
          return response;
        }
        last = new IOException("retryable status " + response.status());
      } catch (IOException e) {
        last = e;
      }
      if (!policy.shouldRetry(attempt, last)) {
        break;
      }
      sleep(backoff.nextDelay(attempt));
    }
    throw new RetryException("gave up on " + request.url(), policy.maxAttempts(), last);
  }

  private static void sleep(Duration duration) {
    try {
      Thread.sleep(duration.toMillis());
    } catch (InterruptedException e) {
      Thread.currentThread().interrupt();
    }
  }
}
