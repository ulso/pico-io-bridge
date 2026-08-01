#!/usr/bin/env python3
"""Read the managed HibouAir catalog through pico-io-bridge SCPI."""

from __future__ import annotations

import argparse
import socket


FIELDS = (
    "id",
    "type",
    "age_ms",
    "reports",
    "temperature_c",
    "humidity_percent",
    "pressure_hpa",
    "co2_ppm",
    "voc_raw",
    "voc_type",
    "noise_db_spl",
    "pm1",
    "pm25",
    "pm10",
    "ambient_light_lx",
)


class ScpiClient:
    def __init__(self, host: str, port: int, timeout: float) -> None:
        self.connection = socket.create_connection((host, port), timeout=timeout)
        self.connection.settimeout(timeout)

    def close(self) -> None:
        self.connection.close()

    def __enter__(self) -> ScpiClient:
        return self

    def __exit__(self, *_: object) -> None:
        self.close()

    def query(self, program: str) -> str:
        self.connection.sendall(program.encode("ascii") + b"\n")
        response = bytearray()
        while b"\n" not in response:
            try:
                chunk = self.connection.recv(1024)
            except (ConnectionResetError, socket.timeout):
                break
            if not chunk:
                break
            response.extend(chunk)
        lines = response.decode("ascii", errors="replace").splitlines()
        if not lines:
            raise RuntimeError(f"No SCPI response to {program!r}")
        return lines[-1]


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--host", default="pico-io-usb-host.local")
    parser.add_argument("--port", type=int, default=5025)
    parser.add_argument("--timeout", type=float, default=4.0)
    parser.add_argument(
        "--filter",
        help="comma-separated six-digit sensor IDs, for example 22005A,22008C",
    )
    parser.add_argument("--clear-filter", action="store_true")
    parser.add_argument("--sensor", help="read only this six-digit sensor ID")
    args = parser.parse_args()

    if args.filter and args.clear_filter:
        parser.error("--filter and --clear-filter are mutually exclusive")

    with ScpiClient(args.host, args.port, args.timeout) as scpi:
        if args.filter:
            response = scpi.query(
                f'SYST:USB:HOST:BLEU:SENS:FILT "{args.filter}"\n'
                "SYST:USB:HOST:BLEU:SENS:FILT?"
            )
            print(f"Filter: {response}")
        elif args.clear_filter:
            response = scpi.query(
                "SYST:USB:HOST:BLEU:SENS:FILT:CLE\n"
                "SYST:USB:HOST:BLEU:SENS:FILT?"
            )
            print(f"Filter: {response}")
        else:
            print("Filter:", scpi.query("SYST:USB:HOST:BLEU:SENS:FILT?"))

        catalog = scpi.query("SYST:USB:HOST:BLEU:SENS:CAT?")
        print(f"Catalog: {catalog}")
        ids = [] if catalog == "NONE" else catalog.split(",")
        if args.sensor:
            ids = [args.sensor.upper().removeprefix("#")]

        for sensor_id in ids:
            data = scpi.query(
                f'SYST:USB:HOST:BLEU:SENS:SEL "{sensor_id}"\n'
                "SYST:USB:HOST:BLEU:SENS:DATA?"
            )
            values = data.split(",")
            if len(values) != len(FIELDS):
                print(f"#{sensor_id}: unexpected response: {data}")
                continue
            snapshot = dict(zip(FIELDS, values, strict=True))
            measurements = ", ".join(
                f"{name}={value}"
                for name, value in snapshot.items()
                if name not in {"id", "type"} and value != "NAN"
            )
            print(f"#{snapshot['id']} {snapshot['type']}: {measurements}")


if __name__ == "__main__":
    try:
        main()
    except (OSError, RuntimeError) as error:
        raise SystemExit(f"HibouAir SCPI query failed: {error}") from None
