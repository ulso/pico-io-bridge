#!/usr/bin/env python3
"""Send one SCPI query through the Pico I/O Bridge USBTMC socket."""

from __future__ import annotations

import argparse
import socket
import sys


def main() -> int:
    parser = argparse.ArgumentParser(
        description="Query a USBTMC instrument connected to Pico I/O Bridge"
    )
    parser.add_argument("--host", default="pico-io-usb-host.local")
    parser.add_argument("--port", type=int, default=5026)
    parser.add_argument("--timeout", type=float, default=10.0)
    parser.add_argument("query", nargs="?", default="*IDN?")
    args = parser.parse_args()

    command = args.query.encode("ascii")
    if not command.endswith(b"\n"):
        command += b"\n"

    try:
        with socket.create_connection(
            (args.host, args.port), timeout=args.timeout
        ) as connection:
            connection.settimeout(args.timeout)
            connection.sendall(command)
            response = bytearray()
            while b"\n" not in response:
                chunk = connection.recv(512)
                if not chunk:
                    break
                response.extend(chunk)
    except (OSError, TimeoutError) as error:
        print(f"USBTMC query failed: {error}", file=sys.stderr)
        return 1

    if not response:
        print("USBTMC instrument returned no data", file=sys.stderr)
        return 1

    print(response.decode("ascii", errors="replace").rstrip())
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
