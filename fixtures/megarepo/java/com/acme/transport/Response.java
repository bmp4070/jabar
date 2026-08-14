package com.acme.transport;

import com.tinyjson.Json;
import com.tinyjson.JsonValue;

/** An inbound HTTP response. Parses its body with the binary-only tinyjson. */
public record Response(int status, String body) {

  public boolean isSuccess() {
    return status >= 200 && status < 300;
  }

  public boolean isRetryable() {
    return status == 429 || status >= 500;
  }

  /** Returns the body parsed as JSON. Resolving this needs the tinyjson jar. */
  public JsonValue json() {
    return Json.parse(body);
  }
}
