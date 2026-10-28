package fallback;
public class Consumer { public String value() { return new Direct().value() + Transitive.value(); } }
