package com.acme.core;

/** The obvious {@link Clock}, backed by {@link System#currentTimeMillis()}. */
public final class SystemClock implements Clock {

  private static final SystemClock INSTANCE = new SystemClock();

  private SystemClock() {}

  public static SystemClock instance() {
    return INSTANCE;
  }

  @Override
  public long nowMillis() {
    return System.currentTimeMillis();
  }
}
