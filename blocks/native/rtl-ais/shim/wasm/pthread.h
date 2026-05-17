/* wasm32-only no-op <pthread.h> for the vendored aisdecoder.
 *
 * aisdecoder.c guards its decoded-message linked-list with a single
 * `pthread_mutex_t message_mutex` (init / lock / unlock / destroy).
 * `wasm32-unknown-unknown` is single-threaded — there is no WASI
 * threads host here and the runtime drives the decoder from one
 * worker — so the mutex degenerates to a no-op. wasi-libc ships no
 * <pthread.h> for this target; this minimal stand-in lets the four
 * call sites compile and fold to nothing.
 *
 * build.rs adds this directory to the include path ONLY for wasm32
 * (via -isystem, ahead of libc-stubs / wasi-libc). Native builds
 * never see this file and use the real system <pthread.h>.
 */
#ifndef FERRITE_WASM_PTHREAD_SHIM_H
#define FERRITE_WASM_PTHREAD_SHIM_H

typedef int pthread_mutex_t;
typedef int pthread_mutexattr_t;

static inline int pthread_mutex_init(pthread_mutex_t *m,
                                     const pthread_mutexattr_t *a) {
    (void)m;
    (void)a;
    return 0;
}
static inline int pthread_mutex_destroy(pthread_mutex_t *m) {
    (void)m;
    return 0;
}
static inline int pthread_mutex_lock(pthread_mutex_t *m) {
    (void)m;
    return 0;
}
static inline int pthread_mutex_unlock(pthread_mutex_t *m) {
    (void)m;
    return 0;
}

#endif /* FERRITE_WASM_PTHREAD_SHIM_H */
