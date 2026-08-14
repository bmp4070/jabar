package com.acme.policy;

import com.acme.core.RetryPolicy;
import com.acme.util.Preconditions;
import com.acme.util.Strings;
import java.util.ArrayList;
import java.util.LinkedHashMap;
import java.util.List;
import java.util.Map;
import java.util.Optional;

/** Name-to-policy lookup. Referenced from several services. */
public final class PolicyRegistry {

  private final Map<String, RetryPolicy> byName = new LinkedHashMap<>();

  public PolicyRegistry register(RetryPolicy policy) {
    Preconditions.checkNotNull(policy, "policy");
    byName.put(policy.name(), policy);
    return this;
  }

  public Optional<RetryPolicy> lookup(String name) {
    if (Strings.isBlank(name)) {
      return Optional.empty();
    }
    return Optional.ofNullable(byName.get(name));
  }

  public String describe() {
    List<String> names = new ArrayList<>(byName.keySet());
    return Strings.commaJoin(names);
  }
}
