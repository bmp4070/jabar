package com.acme.service.inventory;

import com.acme.transport.HttpClient;
import com.acme.transport.Request;
import com.acme.transport.Response;
import com.acme.util.Preconditions;
import com.acme.util.Strings;
import java.io.IOException;
import java.util.List;

/** Reads stock levels. Shallower dependency closure than OrderService. */
public final class InventoryService {

  private final HttpClient http;

  public InventoryService(HttpClient http) {
    this.http = Preconditions.checkNotNull(http, "http");
  }

  public boolean inStock(String sku) throws IOException {
    Preconditions.checkNotNull(sku, "sku");
    Response response = http.send(Request.get("/inventory/" + sku));
    return response.isSuccess();
  }

  public String describeSkus(List<String> skus) {
    return Strings.commaJoin(skus);
  }
}
