package com.acme.service.order;

import com.acme.util.Preconditions;
import java.util.LinkedHashMap;
import java.util.Map;
import java.util.Optional;

/** In-memory order storage. */
public final class OrderRepository {

  private final Map<String, String> ordersById = new LinkedHashMap<>();

  public void save(String orderId, String payload) {
    Preconditions.checkNotNull(orderId, "orderId");
    Preconditions.checkNotNull(payload, "payload");
    ordersById.put(orderId, payload);
  }

  public Optional<String> find(String orderId) {
    return Optional.ofNullable(ordersById.get(orderId));
  }

  public int size() {
    return ordersById.size();
  }
}
