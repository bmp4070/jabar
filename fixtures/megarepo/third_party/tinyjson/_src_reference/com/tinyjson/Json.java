package com.tinyjson;

/** Toy JSON entry point. Stands in for a Maven dependency. */
public final class Json {

  private Json() {}

  public static JsonValue parse(String text) {
    if (text == null || text.isEmpty()) {
      return JsonValue.nullValue();
    }
    return JsonValue.of(text.strip());
  }

  public static String write(JsonValue value) {
    return value.isNull() ? "null" : "\"" + value.asString() + "\"";
  }
}
