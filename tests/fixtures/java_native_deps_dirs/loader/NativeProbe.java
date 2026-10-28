package jni;
public final class NativeProbe {
    static { System.loadLibrary("fixture_native"); }
    private static native int value();
    public static void main(String[] args) { System.out.println("native:" + value()); }
}
