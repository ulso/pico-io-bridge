#!/usr/bin/env python3
"""Discover a BleuIO-equipped Pico I/O Bridge and display one HibouAir sensor."""

from __future__ import annotations

import asyncio
from dataclasses import dataclass
import socket
import threading
import time

import dearpygui.dearpygui as dpg
from zeroconf import ServiceBrowser, ServiceInfo, ServiceListener, Zeroconf


SERVICE_TYPE = "_http._tcp.local."
SCPI_PORT = 5025
SCPI_TIMEOUT = 2.0
POLL_INTERVAL = 1.0
BRIDGE_PROBE_INTERVAL = 3.0
CATALOG_REFRESH_INTERVAL = 5.0
VALUE_SLOT_COUNT = 10

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


@dataclass(frozen=True)
class Bridge:
    service_name: str
    display_name: str
    hostname: str


@dataclass(frozen=True)
class Reading:
    label: str
    value: str


candidate_bridges: dict[str, Bridge] = {}
candidate_lock = threading.Lock()
scpi_lock = threading.Lock()
active_bridges: dict[str, Bridge] = {}
sensor_snapshots: dict[str, dict[str, str]] = {}

selected_bridge = ""
selected_sensor = ""


def service_display_name(service_name: str) -> str:
    suffix = f".{SERVICE_TYPE}"
    return service_name[: -len(suffix)] if service_name.endswith(suffix) else service_name


def is_pico_io_bridge(service_name: str) -> bool:
    return service_display_name(service_name).lower().startswith("pico i/o bridge")


class BridgeListener(ServiceListener):
    """Collect HTTP advertisements without touching Dear PyGui from this thread."""

    def _store(self, zc: Zeroconf, type_: str, name: str) -> None:
        if not is_pico_io_bridge(name):
            return
        info: ServiceInfo | None = zc.get_service_info(type_, name, timeout=2000)
        if info is None or not info.server:
            return
        bridge = Bridge(
            service_name=name,
            display_name=service_display_name(name),
            hostname=info.server.rstrip("."),
        )
        with candidate_lock:
            candidate_bridges[name] = bridge

    def add_service(self, zc: Zeroconf, type_: str, name: str) -> None:
        self._store(zc, type_, name)

    def update_service(self, zc: Zeroconf, type_: str, name: str) -> None:
        self._store(zc, type_, name)

    def remove_service(self, _zc: Zeroconf, _type: str, name: str) -> None:
        with candidate_lock:
            candidate_bridges.pop(name, None)


class ScpiClient:
    def __init__(self, host: str, timeout: float = SCPI_TIMEOUT) -> None:
        self.connection = socket.create_connection((host, SCPI_PORT), timeout=timeout)
        self.connection.settimeout(timeout)

    def __enter__(self) -> ScpiClient:
        return self

    def __exit__(self, *_args: object) -> None:
        self.connection.close()

    def query(self, program: str) -> str:
        self.connection.sendall(program.rstrip("\n").encode("ascii") + b"\n")
        response = bytearray()
        while b"\n" not in response:
            chunk = self.connection.recv(2048)
            if not chunk:
                break
            response.extend(chunk)
        lines = response.decode("ascii", errors="replace").splitlines()
        if not lines:
            raise RuntimeError(f"No SCPI response from {program!r}")
        return lines[-1].strip()


def probe_bleuio(bridge: Bridge) -> bool:
    """Use the USB-host phase, not the sensor catalog, to identify BleuIO."""
    try:
        with scpi_lock, ScpiClient(bridge.hostname) as scpi:
            status = scpi.query("SYST:USB:HOST:STAT?")
        return status.split(",", 1)[0] == "BLEUIO_READY"
    except (OSError, RuntimeError):
        return False


def parse_snapshot(response: str) -> dict[str, str]:
    values = response.split(",")
    if len(values) != len(FIELDS):
        raise RuntimeError(f"Unexpected HibouAir response: {response}")
    return dict(zip(FIELDS, values, strict=True))


def fetch_catalog(hostname: str) -> dict[str, dict[str, str]]:
    snapshots: dict[str, dict[str, str]] = {}
    with scpi_lock, ScpiClient(hostname) as scpi:
        catalog = scpi.query("SYST:USB:HOST:BLEU:SENS:CAT?")
        if catalog == "NONE":
            return snapshots
        for sensor_id in catalog.split(","):
            response = scpi.query(
                f'SYST:USB:HOST:BLEU:SENS:SEL "{sensor_id}"\n'
                "SYST:USB:HOST:BLEU:SENS:DATA?"
            )
            snapshot = parse_snapshot(response)
            snapshots[snapshot["id"]] = snapshot
    return snapshots


def fetch_sensor(hostname: str, sensor_id: str) -> dict[str, str]:
    with scpi_lock, ScpiClient(hostname) as scpi:
        response = scpi.query(
            f'SYST:USB:HOST:BLEU:SENS:SEL "{sensor_id}"\n'
            "SYST:USB:HOST:BLEU:SENS:DATA?"
        )
    return parse_snapshot(response)


def selected_bridge_record() -> Bridge | None:
    return active_bridges.get(selected_bridge)


def sensor_type_label(sensor_type: str) -> str:
    return {
        "CO2": "CO₂",
        "CO2_NOISE": "CO₂ / noise",
        "PARTICULATE": "Particulate matter",
    }.get(sensor_type, sensor_type.replace("_", " ").title())


def sensor_display(sensor_id: str, snapshot: dict[str, str]) -> str:
    return f"#{sensor_id} · {sensor_type_label(snapshot.get('type', 'UNKNOWN'))}"


def sensor_id_from_display(display: str) -> str:
    if not display.startswith("#"):
        return ""
    return display[1:].split(" ", 1)[0]


def available(snapshot: dict[str, str], field: str) -> bool:
    return snapshot.get(field, "NAN") not in ("", "NAN")


def number(snapshot: dict[str, str], field: str, decimals: int = 0) -> str:
    value = float(snapshot[field])
    return f"{value:,.{decimals}f}"


def snapshot_readings(snapshot: dict[str, str]) -> list[Reading]:
    readings: list[Reading] = []

    def add(label: str, field: str, unit: str, decimals: int = 0) -> None:
        if available(snapshot, field):
            suffix = f" {unit}" if unit else ""
            readings.append(Reading(label, f"{number(snapshot, field, decimals)}{suffix}"))

    add("Temperature", "temperature_c", "°C", 1)
    add("Humidity", "humidity_percent", "%", 1)
    add("CO₂", "co2_ppm", "ppm")
    add("Pressure", "pressure_hpa", "hPa")

    if available(snapshot, "voc_raw"):
        if snapshot.get("voc_type") == "2":
            readings.append(Reading("VOC", f"{number(snapshot, 'voc_raw')} IAQ"))
        else:
            readings.append(Reading("VOC raw", number(snapshot, "voc_raw")))

    add("Noise", "noise_db_spl", "dB SPL")
    add("PM1", "pm1", "µg/m³", 1)
    add("PM2.5", "pm25", "µg/m³", 1)
    add("PM10", "pm10", "µg/m³", 1)
    add("Ambient light", "ambient_light_lx", "lx")
    return readings


def clear_sensor_panel(message: str = "No HibouAir sensor selected") -> None:
    dpg.set_value("sensor_kind", "HibouAir sensor")
    dpg.set_value("sensor_id_text", "")
    dpg.set_value("sensor_meta", message)
    for index in range(VALUE_SLOT_COUNT):
        dpg.configure_item(f"reading_group_{index}", show=False)


def update_sensor_panel(snapshot: dict[str, str]) -> None:
    dpg.set_value("sensor_kind", sensor_type_label(snapshot["type"]))
    dpg.set_value("sensor_id_text", f"#{snapshot['id']}")

    age_ms = int(snapshot["age_ms"])
    age_text = "Seen just now" if age_ms < 1500 else f"Seen {age_ms / 1000:.0f} s ago"
    dpg.set_value("sensor_meta", f"{age_text} · {snapshot['reports']} reports")

    readings = snapshot_readings(snapshot)
    for index in range(VALUE_SLOT_COUNT):
        visible = index < len(readings)
        dpg.configure_item(f"reading_group_{index}", show=visible)
        if visible:
            reading = readings[index]
            dpg.set_value(f"reading_label_{index}", reading.label)
            dpg.set_value(f"reading_value_{index}", reading.value)


def update_bridge_combo() -> None:
    global selected_bridge, selected_sensor

    names = sorted(active_bridges)
    dpg.configure_item("bridge_combo", items=names)
    if selected_bridge in names:
        return

    selected_bridge = names[0] if names else ""
    selected_sensor = ""
    sensor_snapshots.clear()
    dpg.set_value("bridge_combo", selected_bridge)
    dpg.configure_item("sensor_combo", items=[])
    dpg.set_value("sensor_combo", "")
    clear_sensor_panel()
    if names:
        dpg.set_value("status_text", "BleuIO found; reading the sensor catalog...")
    else:
        dpg.set_value("status_text", "Searching for a Pico I/O Bridge with BleuIO...")


def update_sensor_combo() -> None:
    global selected_sensor

    displays = [
        sensor_display(sensor_id, snapshot)
        for sensor_id, snapshot in sorted(sensor_snapshots.items())
    ]
    ids = set(sensor_snapshots)
    dpg.configure_item("sensor_combo", items=displays)
    if selected_sensor in ids:
        return

    selected_sensor = sorted(ids)[0] if ids else ""
    if selected_sensor:
        snapshot = sensor_snapshots[selected_sensor]
        dpg.set_value("sensor_combo", sensor_display(selected_sensor, snapshot))
        update_sensor_panel(snapshot)
    else:
        dpg.set_value("sensor_combo", "")
        clear_sensor_panel("BleuIO is ready; waiting for HibouAir advertisements")


def on_bridge_selected(_sender: object, app_data: str, _user_data: object) -> None:
    global selected_bridge, selected_sensor
    selected_bridge = app_data
    selected_sensor = ""
    sensor_snapshots.clear()
    dpg.configure_item("sensor_combo", items=[])
    dpg.set_value("sensor_combo", "")
    clear_sensor_panel("Reading sensor catalog...")
    dpg.set_value("status_text", "Reading sensor catalog...")


def on_sensor_selected(_sender: object, app_data: str, _user_data: object) -> None:
    global selected_sensor
    selected_sensor = sensor_id_from_display(app_data)
    snapshot = sensor_snapshots.get(selected_sensor)
    if snapshot is not None:
        update_sensor_panel(snapshot)


async def probe_loop() -> None:
    """Filter advertised bridges by their SCPI USB-host phase."""
    global active_bridges

    while dpg.is_dearpygui_running():
        with candidate_lock:
            candidates = list(candidate_bridges.values())

        results = await asyncio.gather(
            *(asyncio.to_thread(probe_bleuio, bridge) for bridge in candidates)
        )
        detected = {
            bridge.display_name: bridge
            for bridge, has_bleuio in zip(candidates, results, strict=True)
            if has_bleuio
        }
        if detected != active_bridges:
            active_bridges = detected
            update_bridge_combo()
        await asyncio.sleep(BRIDGE_PROBE_INTERVAL)


async def sensor_loop() -> None:
    """Refresh the catalog occasionally and the selected reading continuously."""
    global sensor_snapshots

    last_catalog_refresh = 0.0
    catalog_bridge = ""
    while dpg.is_dearpygui_running():
        bridge = selected_bridge_record()
        if bridge is None:
            await asyncio.sleep(POLL_INTERVAL)
            continue

        if bridge.hostname != catalog_bridge:
            catalog_bridge = bridge.hostname
            last_catalog_refresh = 0.0

        now = time.monotonic()
        if now - last_catalog_refresh >= CATALOG_REFRESH_INTERVAL:
            try:
                fetched = await asyncio.to_thread(fetch_catalog, bridge.hostname)
                if bridge == selected_bridge_record():
                    sensor_snapshots = fetched
                    update_sensor_combo()
                    if not fetched:
                        dpg.set_value(
                            "status_text",
                            "BleuIO ready; waiting for HibouAir sensors...",
                        )
                last_catalog_refresh = now
            except (OSError, RuntimeError, ValueError) as error:
                dpg.set_value("status_text", f"Catalog error: {error}")

        sensor_id = selected_sensor
        if sensor_id:
            try:
                snapshot = await asyncio.to_thread(fetch_sensor, bridge.hostname, sensor_id)
                if bridge == selected_bridge_record() and sensor_id == selected_sensor:
                    sensor_snapshots[sensor_id] = snapshot
                    update_sensor_panel(snapshot)
                    dpg.set_value("status_text", "Live HibouAir data")
            except (OSError, RuntimeError, ValueError) as error:
                dpg.set_value("status_text", f"Sensor error: {error}")
        await asyncio.sleep(POLL_INTERVAL)


def create_reading_slots() -> None:
    with dpg.table(
        header_row=False,
        borders_innerH=True,
        borders_outerH=False,
        borders_innerV=True,
        borders_outerV=False,
        policy=dpg.mvTable_SizingStretchProp,
    ):
        dpg.add_table_column()
        dpg.add_table_column()
        for row in range(VALUE_SLOT_COUNT // 2):
            with dpg.table_row():
                for column in range(2):
                    index = row * 2 + column
                    with dpg.table_cell():
                        with dpg.group(tag=f"reading_group_{index}", show=False):
                            dpg.add_text("—", tag=f"reading_label_{index}", color=(105, 115, 125))
                            dpg.add_text("—", tag=f"reading_value_{index}")


def create_gui() -> None:
    dpg.create_context()
    dpg.create_viewport(title="Pico I/O Bridge · BleuIO HibouAir", width=780, height=650)

    with dpg.window(label="BleuIO · HibouAir sensor", width=760, height=610):
        dpg.add_text("Pico I/O Bridge · BleuIO", color=(90, 210, 130))
        dpg.add_spacer(height=3)
        dpg.add_combo(
            label="Pico I/O Bridge",
            tag="bridge_combo",
            width=480,
            items=[],
            callback=on_bridge_selected,
        )
        dpg.add_combo(
            label="HibouAir sensor",
            tag="sensor_combo",
            width=340,
            items=[],
            callback=on_sensor_selected,
        )
        dpg.add_text("Searching for a Pico I/O Bridge with BleuIO...", tag="status_text")
        dpg.add_separator()

        with dpg.child_window(height=410, border=True):
            with dpg.group(horizontal=True):
                dpg.add_text("HibouAir sensor", tag="sensor_kind")
                dpg.add_spacer(width=380)
                dpg.add_text("", tag="sensor_id_text", color=(95, 105, 115))
            dpg.add_separator()
            create_reading_slots()
            dpg.add_spacer(height=8)
            dpg.add_text("No HibouAir sensor selected", tag="sensor_meta", color=(105, 115, 125))

    dpg.setup_dearpygui()
    dpg.show_viewport()


async def main_loop() -> None:
    zeroconf = Zeroconf()
    browser = ServiceBrowser(zeroconf, SERVICE_TYPE, BridgeListener())
    tasks = (asyncio.create_task(probe_loop()), asyncio.create_task(sensor_loop()))
    try:
        while dpg.is_dearpygui_running():
            dpg.render_dearpygui_frame()
            await asyncio.sleep(1 / 60)
    finally:
        for task in tasks:
            task.cancel()
        await asyncio.gather(*tasks, return_exceptions=True)
        browser.cancel()
        zeroconf.close()
        dpg.destroy_context()


def main() -> None:
    create_gui()
    asyncio.run(main_loop())


if __name__ == "__main__":
    main()
