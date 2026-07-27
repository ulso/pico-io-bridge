"""Measure environmental values using Pico I/O Bridge and a BME688 sensor.

Install the required packages with:

    python3 -m pip install pyvisa pyvisa-py
"""

import time
from typing import cast

import pyvisa
from pyvisa.resources import MessageBasedResource
from scpi_common import select_visa_resource

SLOT = 5
ADDRESS = 0x77
EXPECTED_DEVICE = f"{SLOT},BME688,0,{ADDRESS}"
INVALID_READING_LIMIT = 9.0e37


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
                    f'SYST:I2C:DEV:ADD {SLOT},"BME688",#H{ADDRESS:02X}'
                )
                error = instrument.query("SYST:ERR?").strip()
                if not error.startswith("0,"):
                    raise RuntimeError(f"Sensor configuration failed: {error}")

            for _ in range(5):
                values = [
                    float(value)
                    for value in instrument.query(f"READ:ENV? {SLOT}").split(",")
                ]
                if len(values) != 4 or any(
                    abs(value) >= INVALID_READING_LIMIT for value in values
                ):
                    print("Environmental measurement unavailable")
                    error = instrument.query("SYST:ERR?").strip()
                    if not error.startswith("0,"):
                        print(f"SCPI: {error}")
                else:
                    temperature, humidity, pressure, gas_resistance = values
                    pressure_hpa = pressure / 100.0
                    print(
                        f"Temperature: {temperature:.2f} C, "
                        f"humidity: {humidity:.2f} %, "
                        f"pressure: {pressure_hpa:.2f} hPa, "
                        f"gas resistance: {gas_resistance:.0f} ohm"
                    )
                time.sleep(1)
    finally:
        resource_manager.close()


if __name__ == "__main__":
    main()
