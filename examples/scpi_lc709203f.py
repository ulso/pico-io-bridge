"""Measure battery voltage and state of charge using an LC709203F.

Install the required packages with:

    python3 -m pip install pyvisa pyvisa-py
"""

import time
from typing import cast

import pyvisa
from pyvisa.resources import MessageBasedResource

VISA_RESOURCE = "TCPIP0::pico-io-can-feather.local::5025::SOCKET"
SLOT = 4
ADDRESS = 0x0B
BATTERY_CAPACITY_MAH = 500
EXPECTED_DEVICE = f"{SLOT},LC709203F,0,{ADDRESS}"


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
                    f'SYST:I2C:DEV:ADD {SLOT},"LC709203F",#H{ADDRESS:02X}'
                )
                error = instrument.query("SYST:ERR?").strip()
                if not error.startswith("0,"):
                    raise RuntimeError(f"Battery monitor configuration failed: {error}")

            _ = instrument.write(
                f"SENS:BATT:CAP {SLOT},{BATTERY_CAPACITY_MAH}"
            )
            error = instrument.query("SYST:ERR?").strip()
            if not error.startswith("0,"):
                raise RuntimeError(f"Battery capacity configuration failed: {error}")

            for _ in range(5):
                voltage = float(instrument.query(f"MEAS:BATT:VOLT? {SLOT}"))
                state_of_charge = float(
                    instrument.query(f"MEAS:BATT:SOC? {SLOT}")
                )
                print(f"Battery: {voltage:.3f} V, {state_of_charge:.1f} %")
                time.sleep(2)
    finally:
        resource_manager.close()


if __name__ == "__main__":
    main()
