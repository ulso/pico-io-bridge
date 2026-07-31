"""Read and optionally exercise a P8055 through Pico I/O Bridge SCPI.

No third-party Python packages are required. The default run is read-only.

Examples:
    python3 examples/scpi_usb_host_p8055.py
    python3 examples/scpi_usb_host_p8055.py --samples 20 --interval 0.2
    python3 examples/scpi_usb_host_p8055.py --pulse-output 1
"""

import argparse
import socket
import sys
import time
from types import TracebackType

from scpi_common import SCPI_PORT

DEFAULT_HOST = "pico-io-usb-host.local"
MAX_RESPONSE_BYTES = 512


class ScpiSocket:
    """One persistent LF-terminated SCPI-RAW connection."""

    def __init__(self, host: str, port: int, timeout: float) -> None:
        self._socket = socket.create_connection((host, port), timeout=timeout)
        self._socket.settimeout(timeout)
        self._reader = self._socket.makefile("rb")

    def __enter__(self) -> "ScpiSocket":
        return self

    def __exit__(
        self,
        _error_type: type[BaseException] | None,
        _error: BaseException | None,
        _traceback: TracebackType | None,
    ) -> None:
        self._reader.close()
        self._socket.close()

    @staticmethod
    def _line(command: str) -> bytes:
        if "\r" in command or "\n" in command:
            raise ValueError("SCPI commands must not contain CR or LF")
        return command.encode("ascii") + b"\n"

    def command(self, command: str) -> None:
        self._socket.sendall(self._line(command))

    def query(self, command: str) -> str:
        self.command(command)
        response = self._reader.readline(MAX_RESPONSE_BYTES + 1)
        if not response:
            raise ConnectionError(f"SCPI connection closed during {command!r}")
        if len(response) > MAX_RESPONSE_BYTES or not response.endswith(b"\n"):
            raise ValueError("SCPI response is too long or lacks an LF terminator")
        return response.decode("ascii").strip()


def parse_uint_csv(response: str, field_count: int, name: str) -> tuple[int, ...]:
    try:
        values = tuple(int(field) for field in response.split(","))
    except ValueError as error:
        raise ValueError(f"invalid {name} response: {response!r}") from error
    if len(values) != field_count or any(value < 0 for value in values):
        raise ValueError(f"invalid {name} response: {response!r}")
    return values


def require_p8055(status: str) -> None:
    fields = status.split(",")
    if len(fields) < 9:
        raise RuntimeError(f"unexpected USB host status: {status!r}")
    phase, speed = fields[:2]
    try:
        vendor_id = int(fields[3])
        product_id = int(fields[4])
        max_transfer = int(fields[8])
    except ValueError as error:
        raise RuntimeError(f"unexpected USB host status: {status!r}") from error
    if (
        phase != "P8055_READY"
        or speed != "LOW"
        or vendor_id != 0x10CF
        or product_id not in range(0x5500, 0x5504)
        or max_transfer != 8
    ):
        raise RuntimeError(f"P8055 is not ready: {status}")


def scpi_error_is_clear(response: str) -> bool:
    return response.startswith('0,"') or response.startswith('+0,"')


def set_and_verify_output(
    instrument: ScpiSocket,
    expected: tuple[int, int, int],
) -> None:
    instrument.command(
        f"SYST:USB:HOST:P8055:OUTP {expected[0]},{expected[1]},{expected[2]}"
    )
    error = instrument.query("SYST:ERR?")
    if not scpi_error_is_clear(error):
        raise RuntimeError(f"P8055 output command failed: {error}")
    confirmed = parse_uint_csv(
        instrument.query("SYST:USB:HOST:P8055:OUTP?"),
        3,
        "P8055 output",
    )
    if confirmed != expected:
        raise RuntimeError(f"output verification failed: {confirmed} != {expected}")


def pulse_output(
    instrument: ScpiSocket,
    output_number: int,
    pulse_seconds: float,
) -> None:
    original = parse_uint_csv(
        instrument.query("SYST:USB:HOST:P8055:OUTP?"),
        3,
        "P8055 output",
    )
    pulse = (
        original[0] ^ (1 << (output_number - 1)),
        original[1],
        original[2],
    )
    output_may_have_changed = False
    try:
        output_may_have_changed = True
        set_and_verify_output(instrument, pulse)
        print(f"Pulsing digital output {output_number} for {pulse_seconds:g} s")
        time.sleep(pulse_seconds)
    finally:
        if output_may_have_changed:
            active_error = sys.exc_info()[0] is not None
            try:
                set_and_verify_output(instrument, original)
                print("Original output state restored and verified")
            except Exception as cleanup_error:
                print(
                    "WARNING: output restoration could not be verified; "
                    f"replug the P8055 before further output commands: {cleanup_error}",
                    file=sys.stderr,
                )
                if not active_error:
                    raise


def read_queued_error(host: str, port: int, timeout: float) -> str | None:
    """Reconnect after a failed query and retrieve its queued SCPI error."""
    for delay in (0.05, 0.1, 0.2):
        time.sleep(delay)
        try:
            with ScpiSocket(host, port, timeout) as instrument:
                return instrument.query("SYST:ERR?")
        except (ConnectionError, OSError, ValueError):
            pass
    return None


def run_session(args: argparse.Namespace) -> None:
    with ScpiSocket(args.host, args.port, args.timeout) as instrument:
        instrument.command("*CLS")
        identity = instrument.query("*IDN?")
        status = instrument.query("SYST:USB:HOST:STAT?")
        require_p8055(status)
        outputs = parse_uint_csv(
            instrument.query("SYST:USB:HOST:P8055:OUTP?"),
            3,
            "P8055 output",
        )

        print(f"Connected to: {identity}")
        print(f"Host status: {status}")
        print(
            "Outputs: "
            f"digital=0x{outputs[0]:02X}, analog1={outputs[1]}, analog2={outputs[2]}"
        )

        if args.pulse_output is not None:
            pulse_output(instrument, args.pulse_output, args.pulse_seconds)

        print("digital  analog1  analog2  counter1  counter2")
        for sample in range(args.samples):
            values = parse_uint_csv(
                instrument.query("SYST:USB:HOST:P8055:INP?"),
                5,
                "P8055 input",
            )
            print(
                f"0b{values[0] & 0x1F:05b}"
                f"{values[1]:9d}{values[2]:9d}{values[3]:10d}{values[4]:10d}"
            )
            if sample + 1 < args.samples:
                time.sleep(args.interval)


def main() -> None:
    parser = argparse.ArgumentParser(
        description="Read a P8055 over Pico I/O Bridge SCPI."
    )
    parser.add_argument("--host", default=DEFAULT_HOST)
    parser.add_argument("--port", type=int, default=SCPI_PORT)
    parser.add_argument("--timeout", type=float, default=6.0)
    parser.add_argument("--samples", type=int, default=5)
    parser.add_argument("--interval", type=float, default=0.5)
    parser.add_argument(
        "--pulse-output",
        type=int,
        choices=range(1, 9),
        metavar="1-8",
        help="opt in to toggling one digital output, then restore it",
    )
    parser.add_argument("--pulse-seconds", type=float, default=0.5)
    args = parser.parse_args()
    if args.samples < 1 or args.interval < 0 or args.pulse_seconds < 0:
        parser.error("samples must be positive and durations must be non-negative")

    try:
        run_session(args)
    except socket.timeout:
        queued_error = read_queued_error(args.host, args.port, args.timeout)
        detail = f"; queued error: {queued_error}" if queued_error else ""
        raise SystemExit(f"SCPI query timed out{detail}") from None
    except (ConnectionError, OSError, UnicodeError, ValueError, RuntimeError) as error:
        raise SystemExit(str(error)) from None


if __name__ == "__main__":
    main()
