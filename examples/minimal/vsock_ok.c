/*
 * vsock_ok — Plan 57 W3 smoke-test guest payload.
 *
 * Built into the rootfs.ext4 produced by examples/minimal/flake.nix.
 * The guest's /init runs us, we connect to the host on AF_VSOCK,
 * write "ok\n", close, and let /init power off the VM. The host-side
 * smoke test (examples/libkrun-smoke.rs) reads "ok" off the Unix
 * socket that libkrun bridges to our vsock port.
 *
 * Single-file static C so the rootfs closure stays tiny — no rustc,
 * no cargo, no glibc-dynamic-linker hop. The Nix flake compiles it
 * with `pkgsStatic.stdenv` so the resulting binary is a self-contained
 * musl-static ELF.
 *
 * Protocol:
 *   - VSOCK_PORT is hardcoded so the host smoke test can match
 *     without round-tripping config through the kernel cmdline.
 *   - We target VMADDR_CID_HOST (CID 2). libkrun routes that to the
 *     Unix socket registered via krun_add_vsock_port on the host.
 *
 * Exit codes:
 *   0  — connect + write succeeded.
 *   1  — socket() failed.
 *   2  — connect() failed (host listener not yet bound, kernel
 *         doesn't have AF_VSOCK, ...).
 *   3  — write() failed.
 */

#include <stdio.h>
#include <string.h>
#include <unistd.h>
#include <errno.h>
#include <sys/socket.h>
#include <linux/vm_sockets.h>

#define VSOCK_PORT 1234
#define VMADDR_CID_HOST 2u

int main(void) {
    int fd = socket(AF_VSOCK, SOCK_STREAM, 0);
    if (fd < 0) {
        fprintf(stderr, "vsock_ok: socket(AF_VSOCK) failed: %s\n", strerror(errno));
        return 1;
    }

    struct sockaddr_vm sa;
    memset(&sa, 0, sizeof(sa));
    sa.svm_family = AF_VSOCK;
    sa.svm_cid = VMADDR_CID_HOST;
    sa.svm_port = VSOCK_PORT;

    if (connect(fd, (const struct sockaddr *)&sa, sizeof(sa)) < 0) {
        fprintf(stderr, "vsock_ok: connect(CID_HOST:%d) failed: %s\n", VSOCK_PORT, strerror(errno));
        return 2;
    }

    const char msg[] = "ok\n";
    ssize_t n = write(fd, msg, sizeof(msg) - 1);
    if (n != (ssize_t)(sizeof(msg) - 1)) {
        fprintf(stderr, "vsock_ok: write returned %zd: %s\n", n, strerror(errno));
        return 3;
    }

    close(fd);
    return 0;
}
