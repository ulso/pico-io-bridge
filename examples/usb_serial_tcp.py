"""Send bytes through the Pico I/O Bridge raw USB-serial TCP port.

No third-party Python packages are required. The default request is ``AT``
followed by CRLF, which is suitable for a connected BleuIO. For that exact
probe, the client reassembles arbitrary stream fragments until the echoed
command and a complete ``OK`` or ``ERROR`` line arrive. Other commands remain
unframed and end when the stream is idle. ``--idle-response`` also selects
idle-delimited collection for ``AT`` when command echo is disabled.

Examples:
    python3 examples/usb_serial_tcp.py
    python3 examples/usb_serial_tcp.py ATI
    python3 examples/usb_serial_tcp.py --hex "00 FF 0D 0A"
"""

import argparse
import socket
from typing import Optional, Union


DEFAULT_HOST = "pico-io-usb-host.local"
DEFAULT_PORT = 7000
DEFAULT_CONNECT_TIMEOUT_SECONDS = 5.0
DEFAULT_FIRST_RESPONSE_TIMEOUT_SECONDS = 10.0
DEFAULT_IDLE_TIMEOUT_SECONDS = 2.0
DEFAULT_MAX_RESPONSE_BYTES = 65536
RECEIVE_CHUNK_BYTES = 1024
BLEUIO_TERMINAL_LINES = {b"OK", b"ERROR"}
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


def line_response_complete(
    response: Union[bytes, bytearray],
    expected_echo: bytes,
) -> bool:
    """Return whether an echoed command is followed by a terminal line."""
    complete_lines = bytes(response).split(b"\r\n")[:-1]
    try:
        echo_index = complete_lines.index(expected_echo)
    except ValueError:
        return False
    return any(
        line in BLEUIO_TERMINAL_LINES
        for line in complete_lines[echo_index + 1 :]
    )


def collect_response(
    connection: socket.socket,
    response_timeout: float,
    idle_timeout: float,
    max_bytes: int,
    *,
    expected_echo: Optional[bytes],
) -> tuple[bytes, bool, bool]:
    """Collect an echo-framed response or an idle-delimited raw response."""
    response = bytearray()
    connection.settimeout(response_timeout)
    complete = False

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
        if expected_echo is not None:
            complete = line_response_complete(response, expected_echo)
            if complete:
                break
        else:
            connection.settimeout(idle_timeout)

    reached_limit = len(response) == max_bytes and (
        expected_echo is None or not complete
    )
    return bytes(response), reached_limit, complete


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
        help=(
            "time to wait for the first byte, or between exact AT response "
            "fragments (default: 10)"
        ),
    )
    parser.add_argument(
        "--idle-timeout",
        type=positive_float,
        default=DEFAULT_IDLE_TIMEOUT_SECONDS,
        metavar="SECONDS",
        help=(
            "idle time ending --hex and non-AT response collection; exact AT "
            "waits for its echo and OK or ERROR (default: 2)"
        ),
    )
    parser.add_argument(
        "--idle-response",
        action="store_true",
        help=(
            "always end response collection on --idle-timeout, including "
            "exact AT; useful when command echo is disabled"
        ),
    )
    parser.add_argument(
        "--max-bytes",
        type=positive_int,
        default=DEFAULT_MAX_RESPONSE_BYTES,
        metavar="COUNT",
        help="maximum response size (default: 65536)",
    )
    args = parser.parse_args()

    command_bytes: Optional[bytes] = None
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
        command_bytes = command.encode("utf-8")
        payload = command_bytes + TERMINATORS[terminator]
        if not payload:
            parser.error("payload must contain at least one byte")

    expected_echo = (
        b"AT"
        if (
            command_bytes == b"AT"
            and payload == b"AT\r\n"
            and not args.idle_response
        )
        else None
    )

    try:
        with socket.create_connection(
            (args.host, args.port),
            timeout=args.connect_timeout,
        ) as connection:
            connection.sendall(payload)
            response, reached_limit, response_complete = collect_response(
                connection,
                args.first_timeout,
                args.idle_timeout,
                args.max_bytes,
                expected_echo=expected_echo,
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
    if expected_echo is not None and not response_complete:
        if reached_limit:
            raise SystemExit(
                "Incomplete AT response: the "
                f"{args.max_bytes}-byte response limit was reached before a "
                "complete echoed AT and OK or ERROR line."
            )
        raise SystemExit(
            "Incomplete AT response: the stream ended or the "
            f"{args.first_timeout:g}-second response timeout expired before a "
            "complete echoed AT and OK or ERROR line."
        )
    if reached_limit:
        print(
            f"Response reached the {args.max_bytes}-byte limit; "
            "more stream data may remain."
        )


if __name__ == "__main__":
    main()
