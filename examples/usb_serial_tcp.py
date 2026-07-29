"""Send bytes through the Pico I/O Bridge raw USB-serial TCP port.

No third-party Python packages are required. The default request is ``AT``
followed by CRLF, which is suitable for a connected BleuIO.

Examples:
    python3 examples/usb_serial_tcp.py
    python3 examples/usb_serial_tcp.py ATI
    python3 examples/usb_serial_tcp.py --hex "00 FF 0D 0A"
"""

import argparse
import socket


DEFAULT_HOST = "pico-io-usb-host.local"
DEFAULT_PORT = 7000
DEFAULT_CONNECT_TIMEOUT_SECONDS = 5.0
DEFAULT_FIRST_RESPONSE_TIMEOUT_SECONDS = 10.0
DEFAULT_IDLE_TIMEOUT_SECONDS = 0.25
DEFAULT_MAX_RESPONSE_BYTES = 65536
RECEIVE_CHUNK_BYTES = 1024
TERMINATORS = {
    "crlf": b"\r\n",
    "cr": b"\r",
    "lf": b"\n",
    "none": b"",
}


def positive_float(value: str) -> float:
    """Parse a strictly positive floating-point command-line value."""
    parsed = float(value)
    if parsed <= 0:
        raise argparse.ArgumentTypeError("must be greater than zero")
    return parsed


def positive_int(value: str) -> int:
    """Parse a strictly positive integer command-line value."""
    parsed = int(value)
    if parsed <= 0:
        raise argparse.ArgumentTypeError("must be greater than zero")
    return parsed


def collect_response(
    connection: socket.socket,
    first_timeout: float,
    idle_timeout: float,
    max_bytes: int,
) -> tuple[bytes, bool]:
    """Collect the first response and then continue until the stream is idle."""
    response = bytearray()
    connection.settimeout(first_timeout)

    while len(response) < max_bytes:
        try:
            chunk = connection.recv(
                min(RECEIVE_CHUNK_BYTES, max_bytes - len(response))
            )
        except socket.timeout:
            break
        except ConnectionResetError:
            if response:
                break
            raise

        if not chunk:
            break

        response.extend(chunk)
        connection.settimeout(idle_timeout)

    return bytes(response), len(response) == max_bytes


def main() -> None:
    parser = argparse.ArgumentParser(
        description=(
            "Send bytes to a CDC-ACM device through the Pico I/O Bridge "
            "raw TCP port."
        )
    )
    parser.add_argument(
        "command",
        nargs="?",
        help='UTF-8 text to send (default: "AT")',
    )
    parser.add_argument(
        "--hex",
        dest="hex_payload",
        metavar="HEX",
        help=(
            "exact hexadecimal bytes to send; whitespace is allowed and no "
            "terminator is appended"
        ),
    )
    parser.add_argument("--host", default=DEFAULT_HOST)
    parser.add_argument("--port", type=int, default=DEFAULT_PORT)
    parser.add_argument(
        "--terminator",
        choices=TERMINATORS,
        default=None,
        help="terminator appended to text commands (default: crlf)",
    )
    parser.add_argument(
        "--connect-timeout",
        type=positive_float,
        default=DEFAULT_CONNECT_TIMEOUT_SECONDS,
        metavar="SECONDS",
    )
    parser.add_argument(
        "--first-timeout",
        type=positive_float,
        default=DEFAULT_FIRST_RESPONSE_TIMEOUT_SECONDS,
        metavar="SECONDS",
        help="time to wait for the first response byte (default: 10)",
    )
    parser.add_argument(
        "--idle-timeout",
        type=positive_float,
        default=DEFAULT_IDLE_TIMEOUT_SECONDS,
        metavar="SECONDS",
        help="idle time that ends response collection (default: 0.25)",
    )
    parser.add_argument(
        "--max-bytes",
        type=positive_int,
        default=DEFAULT_MAX_RESPONSE_BYTES,
        metavar="COUNT",
        help="maximum response size (default: 65536)",
    )
    args = parser.parse_args()

    if args.hex_payload is not None:
        if args.command is not None:
            parser.error("a text command cannot be combined with --hex")
        if args.terminator is not None:
            parser.error("--terminator cannot be combined with --hex")
        try:
            payload = bytes.fromhex(args.hex_payload)
        except ValueError as error:
            parser.error(f"invalid --hex payload: {error}")
        if not payload:
            parser.error("--hex payload must contain at least one byte")
    else:
        command = "AT" if args.command is None else args.command
        terminator = "crlf" if args.terminator is None else args.terminator
        payload = command.encode("utf-8") + TERMINATORS[terminator]
        if not payload:
            parser.error("payload must contain at least one byte")

    try:
        with socket.create_connection(
            (args.host, args.port),
            timeout=args.connect_timeout,
        ) as connection:
            connection.sendall(payload)
            response, reached_limit = collect_response(
                connection,
                args.first_timeout,
                args.idle_timeout,
                args.max_bytes,
            )
    except OSError as error:
        raise SystemExit(
            f"USB-serial TCP connection to {args.host}:{args.port} failed: {error}"
        ) from error

    print(f"Sent {len(payload)} bytes")
    print(f"TX hex:  {payload.hex().upper()}")

    if not response:
        raise SystemExit(
            f"No response received within {args.first_timeout:g} seconds."
        )

    print(f"Received {len(response)} bytes")
    print(f"RX hex:  {response.hex().upper()}")
    print(f"RX text: {response.decode('utf-8', errors='backslashreplace')!r}")
    if reached_limit:
        print(
            f"Response reached the {args.max_bytes}-byte limit; "
            "more stream data may remain."
        )


if __name__ == "__main__":
    main()
