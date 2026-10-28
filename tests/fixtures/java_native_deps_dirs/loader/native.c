#include <jni.h>
JNIEXPORT jint JNICALL Java_jni_NativeProbe_value(JNIEnv *env, jclass cls) {
    (void)env;
    (void)cls;
    return 42;
}
