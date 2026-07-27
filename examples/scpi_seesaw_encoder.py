"""Read an Adafruit seesaw rotary encoder using Pico I/O Bridge.

Install the required packages with:

    python3 -m pip install pyvisa pyvisa-py
"""

import time
from typing import cast

import pyvisa
from pyvisa.resources import MessageBasedResource
from scpi_common import select_visa_resource

SLOT = 6
ADDRESS = 0x36
EXPECTED_DEVICE = f"{SLOT},SEESAW_ENCODER,0,{ADDRESS}"


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
                    f'SYST:I2C:DEV:ADD {SLOT},"SEESAW_ENCODER",#H{ADDRESS:02X}'
                )
                error = instrument.query("SYST:ERR?").strip()
                if not error.startswith("0,"):
                    raise RuntimeError(f"Encoder configuration failed: {error}")

            for _ in range(20):
                delta = int(instrument.query(f"MEAS:ENC:DELTA? {SLOT}"))
                position = int(instrument.query(f"MEAS:ENC:POS? {SLOT}"))
                pressed = bool(int(instrument.query(f"MEAS:ENC:BUTTON? {SLOT}")))
                button = "pressed" if pressed else "released"
                print(f"Position: {position}, delta: {delta:+d}, button: {button}")
                time.sleep(0.25)
    finally:
        resource_manager.close()


if __name__ == "__main__":
    main()
