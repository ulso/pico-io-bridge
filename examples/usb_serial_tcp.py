"""Send bytes through the Pico I/O Bridge raw USB-serial TCP port.

No third-party Python packages are required. The default request is ``AT``
followed by CR, which is suitable for a connected BleuIO. For that exact
probe, the client reassembles arbitrary stream fragments until the echoed
command and a complete ``OK`` or ``ERROR`` line arrive. Other commands remain
unframed and end when the stream is idle. ``--idle-response`` also selects
idle-delimited collection for ``AT`` when command echo is disabled.

Examples:
    python3 examples/usb_serial_tcp.py --host pico-io-usb-host-635b2c.local
    python3 examples/usb_serial_tcp.py --host pico-io-usb-host-635b2c.local ATI
    python3 examples/usb_serial_tcp.py --host pico-io-usb-host-635b2c.local --hex "00 FF 0D 0A"
"""

import argparse
import json
import socket
import time
from typing import Optional, Union


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


def non_negative_float(value: str) -> float:
    """Parse a non-negative floating-point command-line value."""
    parsed = float(value)
    if parsed < 0:
        raise argparse.ArgumentTypeError("must be zero or greater")
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
        echo_index = -1
    if echo_index >= 0 and any(
        line in BLEUIO_TERMINAL_LINES
        for line in complete_lines[echo_index + 1 :]
    ):
        return True

    # BleuIO managed scanning uses verbose JSON mode. A raw TCP client can
    # arrive while that mode is still active, so accept the corresponding
    # command/answer/end envelope as one complete AT response too.
    command_id: Optional[int] = None
    answer_ok = False
    for line in complete_lines:
        try:
            message = json.loads(line)
        except (json.JSONDecodeError, UnicodeDecodeError):
            continue
        if not isinstance(message, dict):
            continue
        if message.get("cmd") == expected_echo.decode("ascii", errors="ignore"):
            command_id = message.get("C")
            answer_ok = False
        elif command_id is not None and message.get("A") == command_id:
            answer_ok = message.get("err") == 0
        elif command_id is not None and message.get("E") == command_id:
            return answer_ok
    return False


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


def print_exchange(
    payload: bytes,
    response: bytes,
    reached_limit: bool,
    response_complete: bool,
    *,
    expected_echo: Optional[bytes],
    first_timeout: float,
    max_bytes: int,
) -> None:
    """Print and validate one request/response exchange."""
    print(f"Sent {len(payload)} bytes")
    print(f"TX hex:  {payload.hex().upper()}")

    if not response:
        raise SystemExit(
            f"No response received within {first_timeout:g} seconds."
        )

    print(f"Received {len(response)} bytes")
    print(f"RX hex:  {response.hex().upper()}")
    print(f"RX text: {response.decode('utf-8', errors='backslashreplace')!r}")
    if expected_echo is not None and not response_complete:
        if reached_limit:
            raise SystemExit(
                "Incomplete AT response: the "
                f"{max_bytes}-byte response limit was reached before a "
                "complete echoed AT and OK or ERROR line."
            )
        raise SystemExit(
            "Incomplete AT response: the stream ended or the "
            f"{first_timeout:g}-second response timeout expired before a "
            "complete echoed AT and OK or ERROR line."
        )
    if reached_limit:
        print(
            f"Response reached the {max_bytes}-byte limit; "
            "more stream data may remain."
        )


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
    parser.add_argument("--host", required=True, help="UID-suffixed hostname or IP address")
    parser.add_argument("--port", type=int, default=DEFAULT_PORT)
    parser.add_argument(
        "--terminator",
        choices=TERMINATORS,
        default=None,
        help="terminator appended to text commands (default: cr)",
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
    parser.add_argument(
        "--repeat",
        type=positive_int,
        default=1,
        metavar="COUNT",
        help=(
            "send the same request COUNT times over one persistent TCP "
            "connection (default: 1)"
        ),
    )
    parser.add_argument(
        "--repeat-delay",
        type=non_negative_float,
        default=0.0,
        metavar="SECONDS",
        help=(
            "wait SECONDS between repeated requests while keeping the TCP "
            "connection open (default: 0)"
        ),
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
        terminator = "cr" if args.terminator is None else args.terminator
        command_bytes = command.encode("utf-8")
        payload = command_bytes + TERMINATORS[terminator]
        if not payload:
            parser.error("payload must contain at least one byte")

    expected_echo = (
        b"AT"
        if (
            command_bytes == b"AT"
            and payload in (b"AT\r", b"AT\r\n")
            and not args.idle_response
        )
        else None
    )

    try:
        with socket.create_connection(
            (args.host, args.port),
            timeout=args.connect_timeout,
        ) as connection:
            for exchange_index in range(args.repeat):
                if args.repeat > 1:
                    if exchange_index:
                        print()
                    print(f"Exchange {exchange_index + 1}/{args.repeat}")
                connection.sendall(payload)
                response, reached_limit, response_complete = collect_response(
                    connection,
                    args.first_timeout,
                    args.idle_timeout,
                    args.max_bytes,
                    expected_echo=expected_echo,
                )
                print_exchange(
                    payload,
                    response,
                    reached_limit,
                    response_complete,
                    expected_echo=expected_echo,
                    first_timeout=args.first_timeout,
                    max_bytes=args.max_bytes,
                )
                if exchange_index + 1 < args.repeat and args.repeat_delay:
                    time.sleep(args.repeat_delay)
    except OSError as error:
        raise SystemExit(
            f"USB-serial TCP connection to {args.host}:{args.port} failed: {error}"
        ) from error


if __name__ == "__main__":
    main()
