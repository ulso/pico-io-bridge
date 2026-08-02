#!/usr/bin/env python3
"""Control a DLP-IOR4 relay module through pico-io-bridge."""

from __future__ import annotations

import argparse
import socket
import sys
import time

BAUD_RATE = 9600
PING_COMMAND = b"'"
PING_RESPONSE = b"R"
RELAY_A_COMMANDS = b"1234"
RELAY_B_COMMANDS = b"QWER"


def configure_ftdi(host: str, port: int, timeout: float) -> None:
    request = (
        "*CLS\n"
        f"SYST:USB:HOST:FTDI:BAUD {BAUD_RATE}\n"
        "SYST:USB:HOST:FTDI:BAUD?\n"
        "SYST:ERR?\n"
    ).encode()
    deadline = time.monotonic() + timeout
    last_lines: list[str] = []

    while True:
        response = bytearray()
        remaining = deadline - time.monotonic()
        if remaining <= 0:
            raise RuntimeError(f"could not configure FTDI baud: {last_lines!r}")

        with socket.create_connection((host, port), timeout=remaining) as connection:
            connection.settimeout(remaining)
            connection.sendall(request)
            while response.count(b"\n") < 2:
                chunk = connection.recv(256)
                if not chunk:
                    break
                response.extend(chunk)

        last_lines = response.decode(errors="replace").splitlines()
        if (
            len(last_lines) >= 2
            and last_lines[0].strip() == str(BAUD_RATE)
            and last_lines[1].lstrip("+").startswith('0,"')
        ):
            return
        if len(last_lines) >= 2 and last_lines[1].startswith('-221,'):
            time.sleep(0.1)
            continue
        raise RuntimeError(f"unexpected FTDI baud response: {last_lines!r}")


def relay_command(relay: int, position: str) -> bytes:
    commands = RELAY_A_COMMANDS if position == "A" else RELAY_B_COMMANDS
    return bytes((commands[relay - 1],))


def ping(host: str, port: int, timeout: float) -> None:
    with socket.create_connection((host, port), timeout=timeout) as connection:
        connection.settimeout(timeout)
        connection.sendall(PING_COMMAND)
        response = connection.recv(1)
    if response != PING_RESPONSE:
        raise RuntimeError(
            f"DLP-IOR4 ping returned {response.hex().upper() or 'no data'}, expected 52"
        )
    print("DLP-IOR4 ping OK: R (0x52)")


def set_relay(
    host: str,
    port: int,
    timeout: float,
    relay: int,
    position: str,
    settle: float,
) -> None:
    command = relay_command(relay, position)
    with socket.create_connection((host, port), timeout=timeout) as connection:
        connection.sendall(command)
        time.sleep(settle)
    print(f"Relay {relay} commanded to position {position} (0x{command[0]:02X})")


def cycle_relay(
    host: str,
    port: int,
    timeout: float,
    relay: int,
    delay: float,
    settle: float,
) -> None:
    with socket.create_connection((host, port), timeout=timeout) as connection:
        command_a = relay_command(relay, "A")
        command_b = relay_command(relay, "B")
        connection.sendall(command_a)
        print(f"Relay {relay} → A")
        time.sleep(delay)
        connection.sendall(command_b)
        print(f"Relay {relay} → B")
        time.sleep(settle)


def main() -> int:
    parser = argparse.ArgumentParser(
        description="Control a DLP-IOR4 connected to pico-io-bridge; test only with safe low voltages"
    )
    parser.add_argument("--host", required=True, help="UID-suffixed hostname or IP address")
    parser.add_argument("--port", type=int, default=7000)
    parser.add_argument("--scpi-port", type=int, default=5025)
    parser.add_argument("--timeout", type=float, default=3.0)
    parser.add_argument(
        "--no-configure",
        action="store_true",
        help="do not set and verify the required 9600 baud over SCPI",
    )
    parser.add_argument(
        "--settle",
        type=float,
        default=0.1,
        help="seconds to keep TCP open after the final relay command",
    )

    actions = parser.add_subparsers(dest="action", required=True)
    actions.add_parser("ping", help="send the DLP-IOR4 ping command")

    set_parser = actions.add_parser("set", help="command one relay to A or B")
    set_parser.add_argument("relay", type=int, choices=range(1, 5))
    set_parser.add_argument("position", choices=("A", "B"))

    cycle_parser = actions.add_parser("cycle", help="command one relay to A and then B")
    cycle_parser.add_argument("relay", type=int, choices=range(1, 5))
    cycle_parser.add_argument("--delay", type=float, default=1.0)

    args = parser.parse_args()
    if args.timeout <= 0 or args.settle < 0:
        parser.error("--timeout must be positive and --settle must be non-negative")
    if args.action == "cycle" and args.delay < 0:
        parser.error("--delay must be non-negative")

    try:
        if not args.no_configure:
            configure_ftdi(args.host, args.scpi_port, args.timeout)
        if args.action == "ping":
            ping(args.host, args.port, args.timeout)
        elif args.action == "set":
            set_relay(
                args.host,
                args.port,
                args.timeout,
                args.relay,
                args.position,
                args.settle,
            )
        else:
            cycle_relay(
                args.host,
                args.port,
                args.timeout,
                args.relay,
                args.delay,
                args.settle,
            )
    except (ConnectionError, OSError, RuntimeError) as error:
        print(f"DLP-IOR4 operation failed: {error}", file=sys.stderr)
        return 1

    return 0


if __name__ == "__main__":
    raise SystemExit(main())
