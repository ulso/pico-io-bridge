#!/usr/bin/env python3
"""Discover Pico I/O Bridges and read a selected ADC channel with PyVISA."""

from __future__ import annotations

import asyncio
from dataclasses import dataclass
import threading

import dearpygui.dearpygui as dpg
import pyvisa
from pyvisa.resources import MessageBasedResource
from zeroconf import ServiceBrowser, ServiceInfo, ServiceListener, Zeroconf


SERVICE_TYPE = "_http._tcp.local."
SCPI_PORT = 5025


@dataclass(frozen=True)
class Bridge:
    service_name: str
    display_name: str
    hostname: str


bridges: dict[str, Bridge] = {}
bridges_lock = threading.Lock()
bridges_changed = threading.Event()

instrument: MessageBasedResource | None = None
resource_manager: pyvisa.ResourceManager | None = None
running_loop: asyncio.AbstractEventLoop | None = None
zeroconf: Zeroconf | None = None
service_browser: ServiceBrowser | None = None


def display_name(service_name: str) -> str:
    """Return the DNS-SD instance name without its service-type suffix."""
    suffix = f".{SERVICE_TYPE}"
    if service_name.endswith(suffix):
        return service_name[: -len(suffix)]
    return service_name


def is_pico_io_bridge(service_name: str) -> bool:
    """Recognise the HTTP instance names advertised by pico-io-bridge."""
    return display_name(service_name).lower().startswith("pico i/o bridge")


class BridgeServiceListener(ServiceListener):
    """Maintain a thread-safe catalog from Zeroconf callback threads."""

    def _store(self, zc: Zeroconf, type_: str, name: str) -> None:
        if not is_pico_io_bridge(name):
            return

        info: ServiceInfo | None = zc.get_service_info(type_, name, timeout=2000)
        if info is None or not info.server:
            return

        bridge = Bridge(
            service_name=name,
            display_name=display_name(name),
            hostname=info.server.rstrip("."),
        )
        with bridges_lock:
            bridges[name] = bridge
        bridges_changed.set()

    def add_service(self, zc: Zeroconf, type_: str, name: str) -> None:
        self._store(zc, type_, name)

    def update_service(self, zc: Zeroconf, type_: str, name: str) -> None:
        self._store(zc, type_, name)

    def remove_service(self, zc: Zeroconf, type_: str, name: str) -> None:
        with bridges_lock:
            removed = bridges.pop(name, None)
        if removed is not None:
            bridges_changed.set()


def close_instrument() -> None:
    """Close the current VISA session and resource manager."""
    global instrument, resource_manager

    if instrument is not None:
        try:
            instrument.close()
        except Exception:
            pass
        instrument = None

    if resource_manager is not None:
        try:
            resource_manager.close()
        except Exception:
            pass
        resource_manager = None


def connect_instrument(hostname: str) -> tuple[str, int]:
    """Open a PyVISA TCPIP-SOCKET session to a selected bridge."""
    global instrument, resource_manager

    close_instrument()
    address = f"TCPIP0::{hostname}::{SCPI_PORT}::SOCKET"
    try:
        resource_manager = pyvisa.ResourceManager("@py")
        opened = resource_manager.open_resource(
            address,
            read_termination="\n",
            write_termination="\n",
        )
        instrument = opened
        instrument.timeout = 5000
        identity = instrument.query("*IDN?").strip()
        channel_count = int(instrument.query("SYST:CHAN:COUN?").strip())
        if channel_count < 1:
            raise ValueError(f"Invalid channel count: {channel_count}")
        return f"Connected: {identity}", channel_count
    except Exception as error:
        close_instrument()
        return f"Connection error for {hostname}: {error}", 0


def blocking_visa_query(command: str) -> str:
    """Perform a blocking VISA query outside the GUI event loop."""
    if instrument is None:
        return "Error: no instrument is connected"
    try:
        return instrument.query(command).strip()
    except Exception as error:
        return f"Communication error: {error}"


async def connect_selected(display: str) -> None:
    """Connect to the bridge selected in the combo box."""
    with bridges_lock:
        bridge = next(
            (item for item in bridges.values() if item.display_name == display),
            None,
        )
    if bridge is None:
        dpg.set_value("status_text", "The selected device is no longer available")
        return

    dpg.set_value("status_text", f"Connecting to {bridge.hostname}...")
    dpg.configure_item("measure_btn", enabled=False)
    status, channel_count = await asyncio.to_thread(connect_instrument, bridge.hostname)
    dpg.set_value("status_text", status)
    channels = [str(channel) for channel in range(channel_count)]
    dpg.configure_item(
        "channel_combo",
        items=channels,
        enabled=bool(channels),
    )
    if channels:
        dpg.set_value("channel_combo", channels[0])
        dpg.set_value("voltage_input", "0.00 V")
    else:
        dpg.set_value("channel_combo", "")
    dpg.configure_item("measure_btn", enabled=bool(channels))


async def measure_callback() -> None:
    """Read the selected ADC channel without blocking Dear PyGui rendering."""
    channel = dpg.get_value("channel_combo")
    if channel == "":
        dpg.set_value("status_text", "No channel is selected")
        return

    dpg.set_value("status_text", "Measuring...")
    dpg.configure_item("measure_btn", enabled=False)

    result = await asyncio.to_thread(
        blocking_visa_query,
        f":MEAS:VOLT:DC? {channel}",
    )

    if not result.lower().startswith(("error:", "communication error:")):
        try:
            result = f"{float(result):.2f} V"
        except ValueError:
            result = f"{result} V"
        dpg.set_value("status_text", f"Measurement complete · channel {channel}")
    else:
        dpg.set_value("status_text", result)
    dpg.set_value("voltage_input", result)
    dpg.configure_item("measure_btn", enabled=instrument is not None)


def schedule(coroutine: object) -> None:
    """Schedule a coroutine from a Dear PyGui callback."""
    if running_loop is not None:
        running_loop.call_soon_threadsafe(
            lambda: asyncio.create_task(coroutine)  # type: ignore[arg-type]
        )


def trigger_connect(_sender: object, app_data: str) -> None:
    schedule(connect_selected(app_data))


def trigger_measure() -> None:
    schedule(measure_callback())


def refresh_bridge_list() -> None:
    """Apply Zeroconf catalog changes on Dear PyGui's main thread."""
    bridges_changed.clear()
    with bridges_lock:
        names = sorted(bridge.display_name for bridge in bridges.values())

    selected = dpg.get_value("device_combo")
    dpg.configure_item("device_combo", items=names)

    if selected in names:
        return

    close_instrument()
    dpg.configure_item("measure_btn", enabled=False)
    dpg.configure_item("channel_combo", items=[], enabled=False)
    dpg.set_value("channel_combo", "")
    if not names:
        dpg.set_value("device_combo", "")
        dpg.set_value("status_text", "Searching for Pico I/O Bridge devices...")
        return

    selected = names[0]
    dpg.set_value("device_combo", selected)
    asyncio.create_task(connect_selected(selected))


def create_gui() -> None:
    dpg.create_context()
    dpg.create_viewport(title="Pico I/O Bridge · PyVISA", width=620, height=330)

    with dpg.window(label="Instrument Panel", width=600, height=290):
        dpg.add_combo(
            label="Pico I/O Bridge",
            tag="device_combo",
            items=[],
            width=390,
            callback=trigger_connect,
        )
        dpg.add_text("Searching for Pico I/O Bridge devices...", tag="status_text")
        dpg.add_spacer(height=10)
        dpg.add_combo(
            label="ADC channel",
            tag="channel_combo",
            items=[],
            width=100,
            enabled=False,
        )
        dpg.add_input_text(
            label="Voltage",
            tag="voltage_input",
            default_value="0.00 V",
            readonly=True,
            width=200,
        )
        dpg.add_spacer(height=10)
        dpg.add_button(
            label="Measure voltage",
            tag="measure_btn",
            callback=trigger_measure,
            enabled=False,
        )

    dpg.setup_dearpygui()
    dpg.show_viewport()


async def main_loop() -> None:
    """Run Dear PyGui and Zeroconf together in one asyncio application."""
    global running_loop, zeroconf, service_browser

    running_loop = asyncio.get_running_loop()
    zeroconf = Zeroconf()
    service_browser = ServiceBrowser(
        zeroconf,
        SERVICE_TYPE,
        BridgeServiceListener(),
    )
    bridges_changed.set()

    try:
        while dpg.is_dearpygui_running():
            if bridges_changed.is_set():
                refresh_bridge_list()
            dpg.render_dearpygui_frame()
            await asyncio.sleep(0.001)
    finally:
        close_instrument()
        if service_browser is not None:
            service_browser.cancel()
        if zeroconf is not None:
            zeroconf.close()
        dpg.destroy_context()


if __name__ == "__main__":
    create_gui()
    asyncio.run(main_loop())
