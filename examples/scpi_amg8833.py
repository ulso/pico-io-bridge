"""Read an AMG8833 thermal frame through Pico I/O Bridge and PyVISA.

Install the required packages with:

    python3 -m pip install pyvisa pyvisa-py
"""

from typing import cast

import pyvisa
from pyvisa.resources import MessageBasedResource
from scpi_common import select_visa_resource

SLOT = 2
ADDRESS = 0x69
EXPECTED_DEVICE = f"{SLOT},AMG8833,0,{ADDRESS}"
PIXELS_PER_ROW = 8
PIXEL_COUNT = PIXELS_PER_ROW**2


def read_frame(instrument: MessageBasedResource) -> list[float]:
    """Read and validate one 8x8 thermal frame."""
    response = instrument.query(f"READ:THERMAL:ARRAY? {SLOT}")
    frame = [float(value) for value in response.split(",")]
    if len(frame) != PIXEL_COUNT:
        raise RuntimeError(f"Expected {PIXEL_COUNT} pixels, received {len(frame)}")
    return frame


def print_frame(frame: list[float]) -> None:
    """Print the thermal frame in the sensor's native pixel order."""
    for row in range(0, PIXEL_COUNT, PIXELS_PER_ROW):
        print(" ".join(f"{value:6.2f}" for value in frame[row : row + PIXELS_PER_ROW]))


def main() -> None:
    visa_resource = select_visa_resource()
    resource_manager = pyvisa.ResourceManager("@py")

    try:
        with resource_manager.open_resource(
            visa_resource,
            read_termination="\n",
            write_termination="\n",
        ) as resource:
            instrument = cast(MessageBasedResource, resource)
            instrument.timeout = 3000
            _ = instrument.write("*CLS")

            identity = instrument.query("*IDN?").strip()
            print(f"Connected to: {identity}")

            configured_devices = instrument.query("SYST:I2C:DEV:LIST?").strip()
            if EXPECTED_DEVICE not in configured_devices.split(";"):
                _ = instrument.write(
                    f'SYST:I2C:DEV:ADD {SLOT},"AMG8833",#H{ADDRESS:02X}'
                )
                error = instrument.query("SYST:ERR?").strip()
                if not error.startswith("0,"):
                    raise RuntimeError(f"Sensor configuration failed: {error}")

            frame = read_frame(instrument)
            print_frame(frame)
            print(
                f"Minimum: {min(frame):.2f} C, "
                f"maximum: {max(frame):.2f} C, "
                f"mean: {sum(frame) / len(frame):.2f} C"
            )
    finally:
        resource_manager.close()


if __name__ == "__main__":
    main()
