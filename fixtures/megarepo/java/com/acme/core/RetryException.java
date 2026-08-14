package com.acme.core;

/** Raised when a {@link RetryPolicy} gives up. */
public class RetryException extends RuntimeException {

  private final int attempts;

  public RetryException(String message, int attempts, Throwable cause) {
    super(message, cause);
    this.attempts = attempts;
  }

  public int attempts() {
    return attempts;
  }
}
