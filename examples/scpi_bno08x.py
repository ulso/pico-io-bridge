"""Read motion and orientation using Pico I/O Bridge and a BNO08x IMU.

Install the required packages with:

    python3 -m pip install pyvisa pyvisa-py
"""

import time
from typing import cast

import pyvisa
from pyvisa.resources import MessageBasedResource
from scpi_common import select_visa_resource

SLOT = 7
ADDRESS = 0x4A
EXPECTED_DEVICE = f"{SLOT},BNO08X,0,{ADDRESS}"
INVALID_READING_LIMIT = 9.0e37
ACCURACY_NAMES = ("unreliable", "low", "medium", "high")


def accuracy_name(value: float) -> str:
    index = int(value)
    if 0 <= index < len(ACCURACY_NAMES):
        return ACCURACY_NAMES[index]
    return f"unknown ({index})"


def print_measurement(values: list[float]) -> None:
    (
        accel_x,
        accel_y,
        accel_z,
        accel_accuracy,
        gyro_x,
        gyro_y,
        gyro_z,
        gyro_accuracy,
        mag_x,
        mag_y,
        mag_z,
        mag_accuracy,
        quat_i,
        quat_j,
        quat_k,
        quat_real,
        quat_accuracy_radians,
        quat_accuracy,
    ) = values

    print(
        f"Acceleration: ({accel_x:+.3f}, {accel_y:+.3f}, {accel_z:+.3f}) m/s^2 "
        f"[{accuracy_name(accel_accuracy)}]"
    )
    print(
        f"Gyroscope:    ({gyro_x:+.3f}, {gyro_y:+.3f}, {gyro_z:+.3f}) rad/s "
        f"[{accuracy_name(gyro_accuracy)}]"
    )
    print(
        f"Magnetic:     ({mag_x:+.2f}, {mag_y:+.2f}, {mag_z:+.2f}) uT "
        f"[{accuracy_name(mag_accuracy)}]"
    )
    print(
        f"Quaternion:   ({quat_i:+.5f}, {quat_j:+.5f}, "
        f"{quat_k:+.5f}, {quat_real:+.5f}), "
        f"accuracy {quat_accuracy_radians:.4f} rad "
        f"[{accuracy_name(quat_accuracy)}]"
    )


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
            instrument.timeout = 5000
            _ = instrument.write("*CLS")

            identity = instrument.query("*IDN?").strip()
            print(f"Connected to: {identity}")

            configured_devices = instrument.query("SYST:I2C:DEV:LIST?").strip()
            if EXPECTED_DEVICE not in configured_devices.split(";"):
                _ = instrument.write(
                    f'SYST:I2C:DEV:ADD {SLOT},"BNO08X",#H{ADDRESS:02X}'
                )
                error = instrument.query("SYST:ERR?").strip()
                if not error.startswith("0,"):
                    raise RuntimeError(f"IMU configuration failed: {error}")

            for _ in range(5):
                values = [
                    float(value)
                    for value in instrument.query(f"READ:IMU? {SLOT}").split(",")
                ]
                if len(values) != 18 or any(
                    abs(value) >= INVALID_READING_LIMIT for value in values
                ):
                    print("IMU measurement unavailable")
                    error = instrument.query("SYST:ERR?").strip()
                    if not error.startswith("0,"):
                        print(f"SCPI: {error}")
                else:
                    print_measurement(values)
                print()
                time.sleep(0.5)
    finally:
        resource_manager.close()


if __name__ == "__main__":
    main()
