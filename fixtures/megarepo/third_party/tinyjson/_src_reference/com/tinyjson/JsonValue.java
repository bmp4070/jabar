package com.tinyjson;

/** A parsed JSON scalar or container. */
public final class JsonValue {

  private final Object raw;

  private JsonValue(Object raw) {
    this.raw = raw;
  }

  public static JsonValue of(String text) {
    return new JsonValue(text);
  }

  public static JsonValue nullValue() {
    return new JsonValue(null);
  }

  public boolean isNull() {
    return raw == null;
  }

  public String asString() {
    return raw == null ? "" : raw.toString();
  }
}
