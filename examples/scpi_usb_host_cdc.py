"""Send one text command through the Pico I/O Bridge PIO USB CDC host.

No third-party Python packages are required.

Examples:
    python3 examples/scpi_usb_host_cdc.py --host pico-io-usb-host-635b2c.local
    python3 examples/scpi_usb_host_cdc.py --host pico-io-usb-host-635b2c.local ATI
    python3 examples/scpi_usb_host_cdc.py --host 10.120.39.1 AT
"""

import argparse
import socket
import time

from scpi_common import SCPI_PORT

DEFAULT_READ_LENGTH = 64
DEFAULT_TIMEOUT_SECONDS = 20.0
TERMINATORS = {
    "crlf": b"\r\n",
    "cr": b"\r",
    "lf": b"\n",
    "none": b"",
}


def scpi_exchange(
    host: str,
    port: int,
    command: str,
    timeout: float,
) -> str:
    """Execute one SCPI command using a short-lived TCP connection."""
    with socket.create_connection((host, port), timeout=timeout) as connection:
        connection.settimeout(timeout)
        connection.sendall(command.encode("ascii") + b"\n")

        response = bytearray()
        while True:
            try:
                chunk = connection.recv(256)
            except (ConnectionResetError, socket.timeout):
                break
            if not chunk:
                break
            response.extend(chunk)
            if b"\n" in response:
                break

    return response.decode("ascii").strip()


def scpi_command(host: str, port: int, command: str, timeout: float) -> None:
    """Send a SCPI command that has no response."""
    with socket.create_connection((host, port), timeout=timeout) as connection:
        connection.settimeout(timeout)
        connection.sendall(command.encode("ascii") + b"\n")


def scpi_error(host: str, port: int, timeout: float) -> str:
    """Read the oldest SCPI error."""
    return scpi_exchange(host, port, "SYST:ERR?", timeout)


def main() -> None:
    parser = argparse.ArgumentParser(
        description="Send a text command to a CDC-ACM device on the PIO USB host."
    )
    parser.add_argument(
        "command",
        nargs="?",
        default="AT",
        help='text sent to the CDC device (default: "AT")',
    )
    parser.add_argument("--host", required=True, help="UID-suffixed hostname or IP address")
    parser.add_argument("--port", type=int, default=SCPI_PORT)
    parser.add_argument(
        "--terminator",
        choices=TERMINATORS,
        default="cr",
        help="line ending appended to the CDC command (default: cr)",
    )
    parser.add_argument(
        "--read-length",
        type=int,
        choices=range(1, 65),
        default=DEFAULT_READ_LENGTH,
        metavar="1-64",
    )
    parser.add_argument(
        "--timeout",
        type=float,
        default=DEFAULT_TIMEOUT_SECONDS,
        help="TCP timeout in seconds (default: 20)",
    )
    args = parser.parse_args()

    scpi_command(args.host, args.port, "*CLS", args.timeout)
    time.sleep(0.05)

    status = scpi_exchange(
        args.host,
        args.port,
        "SYST:USB:HOST:STAT?",
        args.timeout,
    )
    print(f"Host status: {status}")
    if not status.startswith("CDC_READY,"):
        raise SystemExit("No CDC-ACM device is ready.")

    payload = args.command.encode("utf-8") + TERMINATORS[args.terminator]
    payload_hex = payload.hex().upper()
    response_hex = scpi_exchange(
        args.host,
        args.port,
        f'SYST:USB:HOST:CDC:EXCH:HEX? "{payload_hex}",{args.read_length}',
        args.timeout,
    )
    if not response_hex:
        raise SystemExit(
            f"CDC exchange failed: {scpi_error(args.host, args.port, args.timeout)}"
        )

    try:
        response = bytes.fromhex(response_hex)
    except ValueError as error:
        raise SystemExit(f"Invalid hex response from bridge: {response_hex!r}") from error

    print(f"Sent {len(payload)} bytes: {payload!r}")
    print(f"Received {len(response)} bytes")
    print(f"Hex:  {response_hex}")
    print(f"Text: {response.decode('utf-8', errors='backslashreplace')!r}")


if __name__ == "__main__":
    main()
