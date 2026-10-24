// テスト用の drmterm に /dev/input を開かせない (キーボードを奪わない)
#define _GNU_SOURCE
#include <dirent.h>
#include <dlfcn.h>
#include <errno.h>
#include <fcntl.h>
#include <stdarg.h>
#include <string.h>
#include <sys/types.h>

static int blocked(const char *p) { return p && strncmp(p, "/dev/input", 10) == 0; }

#define WRAP_OPEN(name)                                                     \
    int name(const char *p, int f, ...) {                                   \
        va_list a; va_start(a, f); int m = va_arg(a, int); va_end(a);       \
        if (blocked(p)) { errno = EACCES; return -1; }                      \
        static int (*real)(const char *, int, ...);                         \
        if (!real) real = dlsym(RTLD_NEXT, #name);                          \
        return real(p, f, m);                                               \
    }
WRAP_OPEN(open)
WRAP_OPEN(open64)

#define WRAP_OPENAT(name)                                                   \
    int name(int d, const char *p, int f, ...) {                            \
        va_list a; va_start(a, f); int m = va_arg(a, int); va_end(a);       \
        if (blocked(p)) { errno = EACCES; return -1; }                      \
        static int (*real)(int, const char *, int, ...);                    \
        if (!real) real = dlsym(RTLD_NEXT, #name);                          \
        return real(d, p, f, m);                                            \
    }
WRAP_OPENAT(openat)
WRAP_OPENAT(openat64)

DIR *opendir(const char *p) {
    if (blocked(p)) { errno = EACCES; return NULL; }
    static DIR *(*real)(const char *);
    if (!real) real = dlsym(RTLD_NEXT, "opendir");
    return real(p);
}
