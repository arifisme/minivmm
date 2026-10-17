#include <linux/vm_sockets.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/socket.h>
#include <unistd.h>

static int write_all(int fd, const char *buf, ssize_t len) {
    while (len > 0) {
        ssize_t n = write(fd, buf, len);
        if (n <= 0)
            return -1;
        buf += n;
        len -= n;
    }
    return 0;
}

static struct sockaddr_vm address(unsigned int cid, unsigned int port) {
    struct sockaddr_vm addr;
    memset(&addr, 0, sizeof(addr));
    addr.svm_family = AF_VSOCK;
    addr.svm_cid = cid;
    addr.svm_port = port;
    return addr;
}

static int echo(unsigned int port) {
    int listener = socket(AF_VSOCK, SOCK_STREAM, 0);
    struct sockaddr_vm addr = address(VMADDR_CID_ANY, port);
    if (listener < 0 || bind(listener, (struct sockaddr *)&addr, sizeof(addr)) < 0 ||
        listen(listener, 8) < 0) {
        perror("vsock listen");
        return 1;
    }
    printf("vsock echo listening on %u\n", port);
    fflush(stdout);
    for (;;) {
        int conn = accept(listener, NULL, NULL);
        if (conn < 0)
            continue;
        char buf[65536];
        ssize_t n;
        while ((n = read(conn, buf, sizeof(buf))) > 0) {
            if (write_all(conn, buf, n) < 0)
                break;
        }
        close(conn);
    }
}

static int send_to_host(unsigned int port, const char *message) {
    int fd = socket(AF_VSOCK, SOCK_STREAM, 0);
    struct sockaddr_vm addr = address(VMADDR_CID_HOST, port);
    if (fd < 0 || connect(fd, (struct sockaddr *)&addr, sizeof(addr)) < 0) {
        perror("vsock connect");
        return 1;
    }
    if (write_all(fd, message, strlen(message)) < 0 || shutdown(fd, SHUT_WR) < 0) {
        perror("vsock send");
        return 1;
    }
    char buf[4096];
    ssize_t n;
    printf("reply: ");
    while ((n = read(fd, buf, sizeof(buf))) > 0)
        fwrite(buf, 1, n, stdout);
    printf("\n");
    close(fd);
    return 0;
}

int main(int argc, char **argv) {
    if (argc == 3 && strcmp(argv[1], "echo") == 0)
        return echo(strtoul(argv[2], NULL, 10));
    if (argc == 4 && strcmp(argv[1], "send") == 0)
        return send_to_host(strtoul(argv[2], NULL, 10), argv[3]);
    fprintf(stderr, "usage: vsock-test echo PORT | vsock-test send PORT MESSAGE\n");
    return 2;
}
