#define _GNU_SOURCE
#include <dlfcn.h>
#include <errno.h>
#include <fcntl.h>
#include <pthread.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/file.h>
#include <sys/socket.h>
#include <sys/un.h>
#include <unistd.h>

static _Thread_local int reject_worker;

static void barrier(const char *reached_name, const char *release_name) {
    const char *reached = getenv(reached_name);
    const char *release = getenv(release_name);
    if (!reached || !release) return;
    int marker = open(reached, O_CREAT | O_WRONLY, 0600);
    if (marker >= 0) close(marker);
    for (int i = 0; i < 10000 && access(release, F_OK) != 0; ++i) usleep(1000);
}

static int api_socket(int fd) {
    const char *target = getenv("HERDR_SOCKET_PATH");
    struct sockaddr_un address = {0};
    socklen_t size = sizeof(address);
    return target && getsockname(fd, (struct sockaddr *)&address, &size) == 0 &&
           address.sun_family == AF_UNIX && strcmp(address.sun_path, target) == 0;
}

static int before_accept(int fd) {
    const char *trigger = getenv("HERDR_REPRO_ACCEPT_TRIGGER");
    if (!trigger || !api_socket(fd) || unlink(trigger) != 0) return 0;
    barrier("HERDR_REPRO_ACCEPT_REACHED", "HERDR_REPRO_ACCEPT_RELEASE");
    errno = EBADF;
    return 1;
}

static void after_accept(int fd, int accepted) {
    const char *trigger = getenv("HERDR_REPRO_WORKER_TRIGGER");
    if (accepted >= 0 && trigger && api_socket(fd) && unlink(trigger) == 0)
        reject_worker = 1;
}

int accept(int fd, struct sockaddr *address, socklen_t *size) {
    int (*real)(int, struct sockaddr *, socklen_t *) = dlsym(RTLD_NEXT, "accept");
    if (before_accept(fd)) return -1;
    int accepted = real(fd, address, size);
    after_accept(fd, accepted);
    return accepted;
}

int accept4(int fd, struct sockaddr *address, socklen_t *size, int flags) {
    int (*real)(int, struct sockaddr *, socklen_t *, int) = dlsym(RTLD_NEXT, "accept4");
    if (before_accept(fd)) return -1;
    int accepted = real(fd, address, size, flags);
    after_accept(fd, accepted);
    return accepted;
}

int pthread_create(pthread_t *thread, const pthread_attr_t *attr,
                   void *(*entry)(void *), void *arg) {
    int (*real)(pthread_t *, const pthread_attr_t *, void *(*)(void *), void *) =
        dlsym(RTLD_NEXT, "pthread_create");
    if (reject_worker) { reject_worker = 0; return EAGAIN; }
    return real(thread, attr, entry, arg);
}

static int handoff_import(void) {
    char args[4096];
    int fd = open("/proc/self/cmdline", O_RDONLY);
    if (fd < 0) return 0;
    ssize_t size = read(fd, args, sizeof(args));
    close(fd);
    return size > 0 && memmem(args, size, "--handoff-import", 16) != NULL;
}

int flock(int fd, int operation) {
    int (*real)(int, int) = dlsym(RTLD_NEXT, "flock");
    const char *trigger = getenv("HERDR_REPRO_HANDOFF_ADMISSION_TRIGGER");
    char descriptor[64], endpoint_lock[1024], actual[1024];
    const char *api = getenv("HERDR_SOCKET_PATH");
    snprintf(descriptor, sizeof(descriptor), "/proc/self/fd/%d", fd);
    ssize_t count = readlink(descriptor, actual, sizeof(actual) - 1);
    if (count >= 0) actual[count] = 0;
    snprintf(endpoint_lock, sizeof(endpoint_lock), "%s.owner.lock", api ? api : "");
    if (operation == LOCK_EX && count >= 0 && strcmp(actual, endpoint_lock) == 0 &&
        trigger && handoff_import() && unlink(trigger) == 0)
        barrier("HERDR_REPRO_HANDOFF_ADMISSION_REACHED", "HERDR_REPRO_HANDOFF_ADMISSION_RELEASE");
    return real(fd, operation);
}

int bind(int fd, const struct sockaddr *address, socklen_t size) {
    int (*real)(int, const struct sockaddr *, socklen_t) = dlsym(RTLD_NEXT, "bind");
    const char *target = getenv("HERDR_SOCKET_PATH");
    const char *trigger = getenv("HERDR_REPRO_BIND_TRIGGER");
    const struct sockaddr_un *local = (const struct sockaddr_un *)address;
    if (address->sa_family == AF_UNIX && target && trigger &&
        strcmp(local->sun_path, target) == 0 && unlink(trigger) == 0)
        barrier("HERDR_REPRO_BIND_REACHED", "HERDR_REPRO_BIND_RELEASE");
    const char *client_failure = getenv("HERDR_REPRO_HANDOFF_CLIENT_FAILURE");
    if (address->sa_family == AF_UNIX && client_failure && handoff_import() &&
        strstr(local->sun_path, "-client.sock") && unlink(client_failure) == 0) {
        errno = EACCES;
        return -1;
    }
    return real(fd, address, size);
}
