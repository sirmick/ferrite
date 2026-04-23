/* Implementation of the hello-world C functions. `hello_add` and
   `hello_sumf` are pure C with no libc deps; `hello_rmsf` pulls in
   `sqrtf` from libm — the libm link is what we're proving on the
   WASM side. See hello.h for the rationale. */

#include "hello.h"
#include <math.h>

int hello_add(int a, int b) {
    return a + b;
}

float hello_sumf(const float *samples, int n) {
    float acc = 0.0f;
    for (int i = 0; i < n; i++) {
        acc += samples[i];
    }
    return acc;
}

float hello_rmsf(const float *samples, int n) {
    if (n <= 0) return 0.0f;
    float sq = 0.0f;
    for (int i = 0; i < n; i++) {
        sq += samples[i] * samples[i];
    }
    return sqrtf(sq / (float)n);
}
