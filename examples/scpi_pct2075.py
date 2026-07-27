"""Measure temperature using Pico I/O Bridge and a PCT2075 sensor.

Install the required packages with:

    python3 -m pip install pyvisa pyvisa-py
"""

import time
from typing import cast

import pyvisa
from pyvisa.resources import MessageBasedResource

VISA_RESOURCE = "TCPIP0::pico-io-kb2040.local::5025::SOCKET"
SLOT = 3
ADDRESS = 0x37
EXPECTED_DEVICE = f"{SLOT},PCT2075,0,{ADDRESS}"


def main() -> None:
    resource_manager = pyvisa.ResourceManager("@py")

    try:
        with resource_manager.open_resource(
            VISA_RESOURCE,
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
                    f'SYST:I2C:DEV:ADD {SLOT},"PCT2075",#H{ADDRESS:02X}'
                )
                error = instrument.query("SYST:ERR?").strip()
                if not error.startswith("0,"):
                    raise RuntimeError(f"Sensor configuration failed: {error}")

            for _ in range(5):
                temperature = float(
                    instrument.query(f"MEAS:TEMP:EXT? {SLOT}")
                )
                print(f"Temperature: {temperature:.3f} C")
                time.sleep(0.5)
    finally:
        resource_manager.close()


if __name__ == "__main__":
    main()
