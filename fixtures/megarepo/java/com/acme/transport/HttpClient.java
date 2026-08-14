package com.acme.transport;

import java.io.IOException;

/** Sends a {@link Request} and returns a {@link Response}. */
public interface HttpClient {
  Response send(Request request) throws IOException;
}
