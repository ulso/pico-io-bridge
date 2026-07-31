#!/usr/bin/env python3
"""Exercise an FTDI TXD-RXD loopback through the raw TCP bridge."""

from __future__ import annotations

import argparse
import random
import socket
import sys
import time


def receive_exact(connection: socket.socket, expected: int) -> bytes:
    received = bytearray()
    while len(received) < expected:
        chunk = connection.recv(expected - len(received))
        if not chunk:
            raise ConnectionError(
                f"TCP stream ended after {len(received)} of {expected} bytes"
            )
        received.extend(chunk)
    return bytes(received)


def main() -> int:
    parser = argparse.ArgumentParser(
        description="Test a TXD-RXD loopback on an FTDI adapter connected to pico-io-bridge"
    )
    parser.add_argument("--host", default="pico-io-usb-host.local")
    parser.add_argument("--port", type=int, default=7000)
    parser.add_argument("--repeat", type=int, default=100)
    parser.add_argument(
        "--length",
        type=int,
        default=257,
        help="bytes per exchange; 257 crosses several FTDI USB packets",
    )
    parser.add_argument("--timeout", type=float, default=10.0)
    parser.add_argument("--repeat-delay", type=float, default=0.01)
    parser.add_argument("--seed", type=int, default=0xF7D1)
    args = parser.parse_args()

    if args.repeat < 1 or args.length < 1:
        parser.error("--repeat and --length must be positive")

    generator = random.Random(args.seed)
    total = 0
    started = time.monotonic()

    try:
        with socket.create_connection((args.host, args.port), args.timeout) as connection:
            connection.settimeout(args.timeout)
            for exchange in range(1, args.repeat + 1):
                payload = bytes(generator.randrange(256) for _ in range(args.length))
                connection.sendall(payload)
                returned = receive_exact(connection, len(payload))
                if returned != payload:
                    mismatch = next(
                        index
                        for index, (sent, received) in enumerate(zip(payload, returned))
                        if sent != received
                    )
                    print(
                        f"Mismatch in exchange {exchange} at byte {mismatch}: "
                        f"sent {payload[mismatch]:02X}, received {returned[mismatch]:02X}",
                        file=sys.stderr,
                    )
                    return 1
                total += len(payload)
                if exchange == 1 or exchange % 10 == 0 or exchange == args.repeat:
                    print(f"Exchange {exchange}/{args.repeat}: {len(payload)} bytes OK")
                if args.repeat_delay:
                    time.sleep(args.repeat_delay)
    except (ConnectionError, OSError) as error:
        print(f"FTDI loopback failed: {error}", file=sys.stderr)
        return 1

    elapsed = time.monotonic() - started
    print(f"Passed: {total} bytes in {elapsed:.2f} s")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
