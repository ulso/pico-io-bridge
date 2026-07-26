"""Query Pico I/O Bridge over SCPI-RAW using PyVISA.

Install the required packages with:
    python3 -m pip install pyvisa pyvisa-py
"""

import pyvisa


def main():
    resource_manager = pyvisa.ResourceManager("@py")
    try:
        with resource_manager.open_resource(
            "TCPIP0::pico-io-can-feather.local::5025::SOCKET",
            read_termination="\n",
            write_termination="\n",
        ) as instrument:
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
