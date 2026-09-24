#!/usr/bin/env python3
# SPDX-FileCopyrightText: 2026 Ryan Moore
# SPDX-License-Identifier: Apache-2.0
"""Capture the first bytes a TLS client sends, for parser test fixtures.

Listens on 127.0.0.1:<port>, accepts one connection, reads until the peer
pauses (the client blocks waiting for a ServerHello that never comes), and
writes everything received to <output>.

    scripts/capture_client_hello.py 9443 out.bin &
    openssl s_client -connect 127.0.0.1:9443 -servername example.com </dev/null
"""

import socket
import sys

# A ClientHello is at most a few tens of KiB; stop well past that.
MAX_CAPTURE_BYTES = 256 * 1024


def main() -> int:
    if len(sys.argv) != 3:
        print(__doc__, file=sys.stderr)
        return 2
    port, output = int(sys.argv[1]), sys.argv[2]
    with socket.create_server(("127.0.0.1", port)) as server:
        conn, _ = server.accept()
        with conn:
            conn.settimeout(2.0)
            data = bytearray()
            try:
                while len(data) < MAX_CAPTURE_BYTES and (
                    chunk := conn.recv(min(65536, MAX_CAPTURE_BYTES - len(data)))
                ):
                    data += chunk
            except TimeoutError:
                pass
    with open(output, "wb") as f:
        f.write(data)
    print(f"captured {len(data)} bytes to {output}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
