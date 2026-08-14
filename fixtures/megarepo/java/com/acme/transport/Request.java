package com.acme.transport;

import com.acme.util.Preconditions;
import java.util.Map;

/** An outbound HTTP request. */
public record Request(String method, String url, Map<String, String> headers, String body) {

  public Request {
    Preconditions.checkNotNull(method, "method");
    Preconditions.checkNotNull(url, "url");
    Preconditions.checkNotNull(headers, "headers");
  }

  public static Request get(String url) {
    return new Request("GET", url, Map.of(), "");
  }

  public static Request post(String url, String body) {
    return new Request("POST", url, Map.of("content-type", "application/json"), body);
  }
}
