package com.acme.i18n;

import com.acme.util.Preconditions;
import java.util.Map;

/**
 * Deliberately full of non-ASCII text.
 *
 * <p>Every offset in this file differs between UTF-8 bytes, UTF-16 code units
 * and codepoints. The emoji below are outside the BMP, so they cost one
 * codepoint, two UTF-16 code units and four UTF-8 bytes each -- which is
 * exactly where a server that conflates the three starts returning ranges that
 * are silently off by a character.
 */
public final class Messages {

  /** Sales are quoted in euros: prices look like "12,50 €". */
  public static final String CURRENCY_SUFFIX = " €";

  /** Emoji are outside the BMP. Offsets after this constant are the test. */
  public static final String RETRY_BANNER = "🔁 retrying…";

  public static final String SUCCESS_BANNER = "✅ 完了";

  private static final Map<String, String> GREETINGS =
      Map.of(
          "en", "Hello",
          "de", "Grüße",
          "ja", "こんにちは",
          "el", "Γειά σου",
          "ar", "مرحبا");

  private Messages() {}

  /** Identifier with a non-ASCII character, which is legal Java. */
  public static String grüße(String locale) {
    Preconditions.checkNotNull(locale, "locale");
    return GREETINGS.getOrDefault(locale, GREETINGS.get("en"));
  }

  public static String retryBanner(int attempt) {
    return RETRY_BANNER + " (" + attempt + ")";
  }
}
