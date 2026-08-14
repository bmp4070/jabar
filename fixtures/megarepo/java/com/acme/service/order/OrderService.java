package com.acme.service.order;

import com.acme.policy.PolicyRegistry;
import com.acme.transport.HttpClient;
import com.acme.transport.Request;
import com.acme.transport.Response;
import com.acme.transport.RetryingHttpClient;
import com.acme.util.Preconditions;
import java.io.IOException;

/**
 * Top of the fixture's cross-target call chain.
 *
 * <p>{@link #placeOrder} -&gt; {@code RetryingHttpClient.send} -&gt;
 * {@code DefaultRetryPolicy.shouldRetry} -&gt; {@code ExponentialBackoff.nextDelay}.
 * An outgoingCalls query from here has to cross three target boundaries.
 */
public final class OrderService {

  private final HttpClient http;
  private final OrderRepository repository;
  private final PolicyRegistry policies;

  public OrderService(HttpClient http, OrderRepository repository, PolicyRegistry policies) {
    this.http = Preconditions.checkNotNull(http, "http");
    this.repository = Preconditions.checkNotNull(repository, "repository");
    this.policies = Preconditions.checkNotNull(policies, "policies");
  }

  public static OrderService withDefaults(HttpClient transport) {
    return new OrderService(
        RetryingHttpClient.withDefaults(transport), new OrderRepository(), new PolicyRegistry());
  }

  public String placeOrder(String orderId, String payload) throws IOException {
    Preconditions.checkNotNull(orderId, "orderId");
    Response response = http.send(Request.post("/orders/" + orderId, payload));
    if (!response.isSuccess()) {
      throw new IOException("order rejected with " + response.status());
    }
    repository.save(orderId, payload);
    return response.json().asString();
  }

  public String policySummary() {
    return policies.describe();
  }
}
