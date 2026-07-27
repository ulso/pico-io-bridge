"""Query Pico I/O Bridge over SCPI-RAW using PyVISA.

Install the required packages with:
    python3 -m pip install pyvisa pyvisa-py
"""

from typing import cast

import pyvisa
from pyvisa.resources import MessageBasedResource
from scpi_common import select_visa_resource


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
            identity = instrument.query("*IDN?")
            voltage = float(instrument.query("MEAS:VOLT:DC? 0"))
            raw = int(instrument.query("MEAS:ADC:RAW? 0"))

            print(f"Connected to: {identity}")
            print(f"ADC channel 0 voltage: {voltage:.3f} V")
            print(f"ADC channel 0 raw value: {raw}")
    finally:
        resource_manager.close()


if __name__ == "__main__":
    main()
