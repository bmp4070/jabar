package com.acme.service.payment;

import com.acme.core.RetryPolicy;
import com.acme.policy.PolicyRegistry;
import com.acme.transport.HttpClient;
import com.acme.transport.Request;
import com.acme.transport.Response;
import com.acme.util.Preconditions;
import java.io.IOException;

/** Charges cards, conservatively. */
public final class PaymentService {

  private final HttpClient http;
  private final RetryPolicy policy;

  public PaymentService(HttpClient http, RetryPolicy policy) {
    this.http = Preconditions.checkNotNull(http, "http");
    this.policy = Preconditions.checkNotNull(policy, "policy");
  }

  public static PaymentService withDefaults(HttpClient http) {
    return new PaymentService(http, new PaymentRetryPolicy(2));
  }

  public String charge(String accountId, long amountCents) throws IOException {
    Preconditions.checkNotNull(accountId, "accountId");
    Response response = http.send(Request.post("/payments", accountId + ":" + amountCents));
    if (!response.isSuccess()) {
      throw new IOException("payment declined with " + response.status());
    }
    return response.json().asString();
  }

  public PolicyRegistry registry() {
    return new PolicyRegistry().register(policy);
  }
}
