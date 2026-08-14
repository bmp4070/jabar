package com.acme.app;

import com.acme.backoff.ExponentialBackoff;
import com.acme.core.RetryPolicy;
import com.acme.generated.BuildVersion;
import com.acme.i18n.Messages;
import com.acme.policy.CircuitBreakerPolicy;
import com.acme.policy.DefaultRetryPolicy;
import com.acme.policy.PolicyRegistry;
import com.acme.service.inventory.InventoryService;
import com.acme.service.order.OrderService;
import com.acme.service.payment.PaymentRetryPolicy;
import com.acme.service.payment.PaymentService;
import com.acme.transport.RetryingHttpClient;
import com.acme.transport.SimpleHttpClient;
import java.io.IOException;
import java.util.List;

/** Walks the whole fixture graph once, so it is steppable under a debugger. */
public final class Main {

  public static void main(String[] args) throws IOException {
    System.out.println(BuildVersion.describe());
    System.out.println(Messages.grüße("de") + Messages.CURRENCY_SUFFIX);
    System.out.println(Messages.retryBanner(1));

    RetryPolicy defaults = DefaultRetryPolicy.defaults();
    RetryPolicy breaker = new CircuitBreakerPolicy(defaults, 5);
    RetryPolicy payments = new PaymentRetryPolicy(2);

    PolicyRegistry registry =
        new PolicyRegistry().register(defaults).register(breaker).register(payments);
    System.out.println("policies: " + registry.describe());
    System.out.println("backoff: " + ExponentialBackoff.defaults().describe());

    SimpleHttpClient raw = new SimpleHttpClient("http://acme.test");
    OrderService orders =
        new OrderService(
            RetryingHttpClient.withDefaults(raw), new com.acme.service.order.OrderRepository(),
            registry);
    System.out.println("order: " + orders.placeOrder("A-1", "{\"sku\":\"widget\"}"));

    InventoryService inventory = new InventoryService(raw);
    System.out.println("in stock: " + inventory.inStock("widget"));
    System.out.println("skus: " + inventory.describeSkus(List.of("widget", "gadget")));

    PaymentService payment = new PaymentService(raw, payments);
    System.out.println("charge: " + payment.charge("acct-9", 1250L));
    System.out.println(Messages.SUCCESS_BANNER);
  }
}
