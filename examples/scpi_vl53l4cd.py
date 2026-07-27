"""Measure distance using Pico I/O Bridge and a VL53L4CD sensor.

Install the required packages with:

    python3 -m pip install pyvisa pyvisa-py
"""

from typing import cast

import pyvisa
from pyvisa.resources import MessageBasedResource
from scpi_common import select_visa_resource

EXPECTED_DEVICE = "1,VL53L4CD,0,41"
INVALID_READING_LIMIT = 9.0e37


def measure_distance(inst: MessageBasedResource) -> float:
    """Get distance from VL53L4CD or return -1 if an error occurs."""
    result = -1.0
    distance = float(inst.query("MEAS:DIST? 1"))

    if abs(distance) >= INVALID_READING_LIMIT:
        while True:
            error = inst.query("SYST:ERR?").strip()
            if error.startswith("0,"):
                break
    else:
        result = distance
    return result


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
                _ = instrument.write('SYST:I2C:DEV:ADD 1,"VL53L4CD",#H29')

                error = instrument.query("SYST:ERR?").strip()
                if not error.startswith("0,"):
                    raise RuntimeError(f"Sensor configuration failed: {error}")

            for _ in range(5):
                dist = measure_distance(instrument)
                print(f"Distance: {dist:.3f} m")
    finally:
        resource_manager.close()


if __name__ == "__main__":
    main()
