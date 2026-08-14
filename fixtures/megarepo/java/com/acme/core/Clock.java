package com.acme.core;

/** Indirection over wall-clock time so retry logic stays testable. */
public interface Clock {
  long nowMillis();
}
