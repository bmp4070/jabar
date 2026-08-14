package com.acme.transport;

import com.acme.util.Preconditions;
import com.acme.util.Strings;
import java.io.IOException;

/** A stand-in transport. Fails on demand so retry paths are exercisable. */
public final class SimpleHttpClient implements HttpClient {

  private final String baseUrl;
  private int calls;

  public SimpleHttpClient(String baseUrl) {
    this.baseUrl = Strings.orDefault(baseUrl, "http://localhost");
  }

  public int calls() {
    return calls;
  }

  @Override
  public Response send(Request request) throws IOException {
    Preconditions.checkNotNull(request, "request");
    calls++;
    if (request.url().contains("flaky") && calls < 3) {
      throw new IOException("transient failure from " + baseUrl);
    }
    return new Response(200, "{\"ok\":true}");
  }
}
