package com.acme.core;

import java.time.Duration;

/** Computes how long to wait before attempt {@code attempt}. */
public interface Backoff {

  /** Delay before the given attempt. Attempt 1 is the first retry. */
  Duration nextDelay(int attempt);

  /** A short identifier for diagnostics. */
  String describe();
}
