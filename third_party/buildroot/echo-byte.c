// SPDX-FileCopyrightText: © 2026 Olof Johansson
// SPDX-License-Identifier: MIT
//
// Unbuffered byte-echo helper for the host-side console roundtrip-
// latency benchmark (#36). Reads from stdin one byte at a time via
// raw read(2) and writes it back via raw write(2) — no FILE*, no
// libc-side buffering. busybox's `head -c1 | printf '%s'` shape
// line-buffers per-byte writes against /dev/hvc0, which made the
// host-side echo timing unmeasurable for sub-line writes (#28's
// SKIP rows).
//
// Usage:
//   echo-byte           — echo until stdin EOF.
//   echo-byte N         — echo exactly N bytes, then exit.
//
// The bench passes a count so the helper terminates cleanly without
// the bench needing to send EOF (which is awkward over /dev/hvc0
// with no controlling tty).

#include <stdio.h>
#include <stdlib.h>
#include <unistd.h>

int main(int argc, char **argv) {
    long limit = -1;  // -1 = unbounded
    if (argc > 1) {
        char *end = NULL;
        limit = strtol(argv[1], &end, 10);
        if (end == argv[1] || *end != '\0' || limit < 0) {
            fprintf(stderr, "echo-byte: invalid count %s\n", argv[1]);
            return 2;
        }
    }
    char c;
    long n = 0;
    while ((limit < 0 || n < limit) && read(0, &c, 1) == 1) {
        if (write(1, &c, 1) != 1) {
            return 1;
        }
        n++;
    }
    return 0;
}
