# Pico I/O Bridge for RP2040

Rust firmware for supported RP2040 boards that exposes their hardware
interfaces over USB CDC-NCM. The board appears as a small USB Ethernet device,
advertises a board-specific `.local` hostname, serves browser consoles, and
accepts bus commands over WebSocket and instrument commands over SCPI-RAW.

A compile-time board profile selects the available interfaces, peripherals,
pins, flash size, status indicator, and USB product name. The default Adafruit
RP2040 CAN Bus Feather profile enables both its onboard CAN controller and
STEMMA QT I2C bus. All profiles enable the four external ADC channels. The
Adafruit Feather RP2040 and KB2040 profiles enable I2C and ADC without
compiling CAN support or configuring CAN/SPI pins.

## Current Features

- USB CDC-NCM Ethernet interface
- Embedded DHCP server using a UID-derived `10.x.y.0/24` subnet
- Stable locally administered MAC addresses derived from the Pico flash UID
- Stable 16-character USB serial number derived from the full 64-bit flash UID
- IPv6 link-local address derived from the device MAC
- mDNS/DNS-SD advertisement for `_http._tcp` and `_scpi-raw._tcp`
- Predictable board-specific hostnames with a flash-UID fallback on conflict
- SCPI-RAW instrument server on TCP port 5025 using `microscpi`
- Four-channel 12-bit ADC measurements on A0-A3, plus internal temperature
- Explicit SCPI configuration and ranging support for VL53L4CD I2C sensors
- AMG8833 8x8 thermal array measurements over SCPI
- Built-in browser CAN and I2C consoles
- WebSocket CAN API at `/can`
- WebSocket I2C API at `/i2c`
- MCP25625 CAN support via the `mcp25xx` crate
- CAN TX, RX broadcast to connected WebSocket clients, status, bitrate/mode config
- Concurrent I2C status, scan, read, write, and write-read transactions
- Compile-time profiles for three Adafruit RP2040 boards
- Board status indication while the USB network is not ready, where available

## Hardware

Supported board profiles:

| Cargo feature | Interfaces | STEMMA QT | mDNS hostname |
| --- | --- | --- | --- |
| `board-adafruit-rp2040-can` | CAN, I2C, ADC | I2C1, SDA GP2, SCL GP3 | `pico-io-can-feather.local` |
| `board-adafruit-feather-rp2040` | I2C, ADC | I2C1, SDA GP2, SCL GP3 | `pico-io-feather.local` |
| `board-adafruit-kb2040` | I2C, ADC | I2C0, SDA GP12, SCL GP13 | `pico-io-kb2040.local` |

The regular Feather RP2040 profile selects Embassy's generic `03h` second-stage
flash bootloader. Adafruit boards of this model may contain either GD25Q64C or
W25Q64JV flash, and the official Pico SDK board definition makes the same
generic compatibility choice. The KB2040 uses the faster W25Q bootloader.

The default Adafruit RP2040 CAN Bus Feather connects its onboard MCP25625 as
follows:

| Signal | Pico pin |
| --- | --- |
| SPI1 SCK | GP14 |
| SPI1 MOSI | GP15 |
| SPI1 MISO | GP8 |
| MCP25625 CS | GP19 |

The default CAN bitrate is 500 kbit/s.

The I2C bus runs at 400 kHz. On the CAN Feather, CAN uses SPI1 while STEMMA QT
uses I2C1, so both hardware blocks run concurrently without a pin conflict.
All profiles expose the RP2040 ADC inputs A0-A3 on GP26-GP29 through SCPI.
Pins for interfaces absent from the selected board profile are left untouched.

The CAN Feather and regular Feather RP2040 use their red LED on GP13 as a
startup indicator. The LED turns on after reset and turns off when DHCP has
assigned the host an address and mDNS has established the advertised services.
The KB2040 profile cannot use GP13 as an LED because it is the STEMMA QT clock
pin. Its onboard NeoPixel is deliberately left untouched for now, so network
readiness is reported through the UART log instead.

If the host remains silent during startup, the firmware briefly disconnects
USB before retrying enumeration. The disconnect grows from 350 ms to 1, 3,
and finally 5 seconds so a host reconnecting an entire dock has time to discard
stale CDC-NCM state. The retry counter survives automatic recovery resets but a
reset-button press or a real link-down starts a fresh sequence. If the LED stays
red after the retry limit, physically unplug the board's USB cable for at least
five seconds; a bus-powered device cannot remove VBUS from its own hub port.

## Build

The project is configured for `thumbv6m-none-eabi` in `.cargo/config.toml`.

```sh
cargo build --release
```

The default feature is:

```text
board-adafruit-rp2040-can
```

This complete board profile enables mDNS, the DHCP server, I2C, and the
MCP25625-backed CAN interface. The lower-level `can`, `i2c`, `mcp2515`, and
`mcp25625` features are implementation building blocks for board profiles and
are not normally selected directly. Exactly one `board-*` feature must be
enabled. Future profiles can add interfaces such as GPIO without changing the
shared CDC-NCM, HTTP, WebSocket, or SCPI layers.

With the default features the Pico uses `10.x.y.1/24` and leases
`10.x.y.2/24` to the USB host. The `x.y` subnet bytes are derived from the
flash UID, so different boards should normally land on different private /24
networks. The DHCP response does not advertise a router or DNS server; `.local`
discovery still comes from mDNS.

For the regular Adafruit Feather RP2040:

```sh
cargo build --release --no-default-features --features board-adafruit-feather-rp2040
```

For the Adafruit KB2040:

```sh
cargo build --release --no-default-features --features board-adafruit-kb2040
```

The two I2C/ADC profiles exclude the `mcp25xx` dependency, CAN task, CAN HTTP
page and `/can` WebSocket endpoint from the resulting firmware.

## Flash

The cargo runner is configured for `elf2uf2-rs`:

```toml
runner = "elf2uf2-rs --deploy --serial --verbose"
```

With the Pico in BOOTSEL mode, this should usually be enough:

```sh
cargo run --release
```

Select a different board while flashing in the same way as while building:

```sh
cargo run --release --no-default-features --features board-adafruit-kb2040
```

## Network Use

All current board profiles use the embedded DHCP server. The device uses its
UID-derived `10.x.y.1/24` address and gives the host `10.x.y.2/24`.

Each physical board reports the full eight-byte flash UID as 16 uppercase
hexadecimal characters in the USB serial-number descriptor. The same UID seeds
the MAC and private subnet derivation, allowing a host to distinguish multiple
boards that run the same profile. The current development VID/PID remains
`0xC0DE:0xCAFE` and must be replaced with an assigned identity before the
firmware is distributed as a USB product.

Each profile advertises its hostname from the hardware table and matching,
board-specific `_http._tcp` and `_scpi-raw._tcp` service instances. The HTTP
TXT record includes `path=/`. The SCPI TXT record includes the manufacturer,
board model, full flash-UID serial number, and firmware version, matching the
fields returned by `*IDN?`. For example, the default profile advertises:

```text
pico-io-can-feather.local
_http._tcp
_scpi-raw._tcp
```

If another board has already claimed the same profile hostname, the conflicting
firmware automatically registers again with the final three flash-UID bytes as
a six-character suffix, for example `pico-io-kb2040-635b2c.local`. Its DNS-SD
service instance receives the same suffix. Thus a single board keeps a short,
predictable CLI name while multiple identical boards remain independently
addressable. `dns-sd -B _http._tcp` and `dns-sd -B _scpi-raw._tcp` show the
active instances. `dns-sd -L <instance> _scpi-raw._tcp local.` resolves an
instrument to its hostname, port, and TXT metadata.

Useful checks for the default profile on macOS:

```sh
dns-sd -B _http._tcp
dns-sd -B _scpi-raw._tcp
dns-sd -G v4v6 pico-io-can-feather.local
ping pico-io-can-feather.local
ping6 pico-io-can-feather.local
curl http://pico-io-can-feather.local/api/status
```

The built-in web UI is available at:

```text
http://pico-io-can-feather.local/          CAN console
http://pico-io-can-feather.local/i2c.html I2C console
http://pico-io-can-feather.local/scpi.html SCPI instrument information
```

Only pages and WebSocket endpoints for interfaces in the selected board profile
are available. The CAN Feather uses the CAN console at `/`; the two I2C-only
profiles serve the I2C console there. The SCPI information page is available in
every profile. `/api/status` reports instrument identity, SCPI-RAW connection
metadata, the active interface names and the endpoint paths in its `pages` and
`websockets` fields.

## SCPI-RAW Instrument API

The SCPI server listens on raw TCP port 5025 and accepts LF-terminated commands.
It processes one instrument session at a time. Install PyVISA and its
pure-Python backend:

```sh
python3 -m pip install pyvisa pyvisa-py
```

A PyVISA socket resource can then be opened with:

```python
import pyvisa

rm = pyvisa.ResourceManager("@py")
instrument = rm.open_resource(
    "TCPIP0::pico-io-can-feather.local::5025::SOCKET",
    read_termination="\n",
    write_termination="\n",
)

print(instrument.query("*IDN?"))
print(instrument.query("MEAS:VOLT:DC? 0"))
print(instrument.query("MEAS:ADC:RAW? 0"))
```

GNU Octave with the `instrument-control` package can use the same raw socket.
Use its line-oriented helpers rather than `read(pico)` without a byte count;
that form only reads the bytes already available and can return before the
instrument has answered:

```octave
pkg load instrument-control;

pico = tcpclient("pico-io-can-feather.local", 5025, "Timeout", 2);
configureTerminator(pico, "lf");

identity = writeread(pico, "*IDN?");
voltage = str2double(writeread(pico, "MEAS:VOLT:DC? 1"));

fprintf("Connected to: %s\n", identity);
fprintf("ADC channel 1: %.3f V\n", voltage);
clear pico;
```

SCPI Commander, an iPhone instrument-control app, has also been tested
successfully with Pico I/O Bridge. The iPhone app can additionally run on Macs
with Apple Silicon, where it has been verified with the same SCPI-RAW interface.

Initial command set:

| Command | Result or action |
| --- | --- |
| `*IDN?` | `manufacturer,model,serial,firmware` |
| `*RST` | Reset SCPI status and error state |
| `*TST?` | ADC self-test, `0` means pass |
| `*CLS`, `*ESR?`, `*STB?`, `*ESE[?]`, `*SRE[?]`, `*OPC[?]` | IEEE 488.2 status handling |
| `SYST:VERS?` | Supported SCPI standard version |
| `SYST:ERR?`, `SYST:ERR:COUN?` | Read the error queue |
| `SYST:CHAN:COUN?` | Number of external ADC channels (`4`) |
| `SYST:I2C:DEV:CAT?` | Supported I2C device models |
| `SYST:I2C:DEV:ADD <slot>,"<model>",<address>` | Verify, initialize, and register a device |
| `SYST:I2C:DEV? <slot>` | Device configuration as `slot,model,bus,address` |
| `SYST:I2C:DEV:LIST?` | All configured devices, or `NONE` |
| `SYST:I2C:DEV:COUN?` | Number of configured devices |
| `SYST:I2C:DEV:DEL <slot>` | Stop and remove a configured device |
| `SYST:I2C:DEV:CLEAR` | Stop and remove all configured devices |
| `SENS:AVER:COUN <count>` | Set the global ADC averaging count (`1`-`256`) |
| `SENS:AVER:COUN?` | Read the global ADC averaging count |
| `MEAS:ADC:RAW? <channel>` | Averaged 12-bit ADC code for channel 0-3 |
| `MEAS:VOLT:DC? <channel>` | Averaged nominal voltage for channel 0-3 |
| `MEAS:TEMP?` | Approximate RP2040 internal temperature in degrees Celsius |
| `MEAS:DIST? <slot>` | Distance in meters from a configured ranging sensor |
| `MEAS:THERM:PIX? <slot>,<pixel>` | AMG8833 pixel 0-63 in degrees Celsius |
| `MEAS:THERM:MIN? <slot>` | Minimum AMG8833 frame temperature |
| `MEAS:THERM:MAX? <slot>` | Maximum AMG8833 frame temperature |
| `MEAS:THERM:AVER? <slot>` | Mean AMG8833 frame temperature |
| `READ:THERM:ARR? <slot>` | All 64 AMG8833 pixels in degrees Celsius |

SCPI channel 0 maps to A0/GP26, through channel 3 at A3/GP29. Voltage conversion
assumes a nominal 3.3 V ADC reference and is not calibrated. Keep analog inputs
between ground and 3.3 V; RP2040 GPIO pins are not 5 V tolerant.

Each measurement takes a fresh block of samples and returns their rounded
arithmetic mean. `SENS:AVER:COUN` controls the block size globally for A0-A3
and the internal temperature sensor. The default is 16 samples; `*RST` restores
that default. Larger values reduce uncorrelated noise but increase measurement
latency proportionally.

### Known I2C Devices

Known devices are configured explicitly because an I2C scan generally reveals
only addresses, not reliable model identities. Eight logical slots are
available. The configuration is kept in RAM, survives `*RST`, and is cleared
when the firmware restarts.

The first supported device is the ST VL53L4CD time-of-flight distance sensor.
For a sensor at its default 7-bit I2C address:

```text
SYST:I2C:DEV:ADD 1,"VL53L4CD",#H29
SYST:I2C:DEV? 1
MEAS:DIST? 1
```

Example responses:

```text
1,VL53L4CD,0,41
0.347
```

`DEV:ADD` verifies the VL53L4CD identity, applies the sensor initialization
sequence, and starts ranging. Before measuring, `MEAS:DIST?` discards any
sample that was already waiting when the command arrived. It then tries up to
three fresh samples, returns the first valid nonzero measurement, and converts
the sensor's millimeter result to meters. If all three samples are invalid or
zero, the query returns the SCPI NaN value `9.91E+37` instead of leaving the
client without a response. It also queues a hardware error that can be
inspected with `SYST:ERR?`.

Only one configured device may use a given address. Supporting multiple
VL53L4CD sensors at reassigned addresses will additionally require control of
their XSHUT pins and is outside the initial implementation.

The Panasonic AMG8833 thermal array is supported at its default address
`0x69` and alternate address `0x68`:

```text
SYST:I2C:DEV:ADD 2,"AMG8833",#H69
MEAS:THERM:PIX? 2,0
MEAS:THERM:MAX? 2
READ:THERM:ARR? 2
```

`DEV:ADD` wakes and resets the sensor, selects its 10 frames-per-second mode,
and clears status flags. Each measurement command reads a fresh 8x8 frame.
Pixels are numbered 0-63 in the sensor's native order and temperatures are
reported in degrees Celsius at the AMG8833's 0.25 degree resolution. The array
query returns all 64 pixels as a comma-separated list.

## Local HTML Apps

Custom control pages do not have to be uploaded to the Pico. A standalone HTML
file on the host can connect directly to any endpoint enabled in the firmware:

```text
ws://pico-io-can-feather.local/can
ws://pico-io-can-feather.local/i2c
```

For example, `examples/led_control.html` can be opened directly in Safari and
uses the WebSocket API to switch a CAN-connected LED node on and off. This keeps
the firmware simple while still allowing project-specific browser tools.

## CAN WebSocket API

WebSocket endpoint:

```text
ws://pico-io-can-feather.local/can
```

On connect, the device sends:

```json
{"type":"hello","ok":true,"endpoint":"/can"}
```

Request CAN status:

```json
{"type":"can.status"}
```

Transmit a standard CAN frame:

```json
{"type":"can.tx","bus":0,"id":291,"ext":false,"rtr":false,"dlc":1,"data":[1]}
```

Transmit an RTR frame:

```json
{"type":"can.tx","bus":0,"id":291,"ext":false,"rtr":true,"dlc":1,"data":[]}
```

Received CAN frames are broadcast to every connected WebSocket client:

```json
{"type":"can.rx","ok":true,"bus":0,"id":291,"ext":false,"rtr":false,"dlc":5,"data":[104,101,108,108,111]}
```

Examples:

- `examples/can_ws.py`: Python WebSocket client
- `examples/led_control.html`: standalone browser LED control page
- `examples/scpi_amg8833.py`: PyVISA AMG8833 8x8 thermal frame reader
- `examples/scpi_vl53l4cd.py`: PyVISA VL53L4CD distance measurement

## I2C WebSocket API

WebSocket endpoint:

```text
ws://pico-io-can-feather.local/i2c
```

On connect, the device sends:

```json
{"type":"hello","ok":true,"endpoint":"/i2c"}
```

Request I2C status:

```json
{"type":"i2c.status"}
```

Scan the normal 7-bit address range:

```json
{"type":"i2c.scan","bus":0}
```

Read two bytes from address `0x18` (decimal 24):

```json
{"type":"i2c.read","bus":0,"address":24,"length":2}
```

Write register address `0x05` to the same device:

```json
{"type":"i2c.write","bus":0,"address":24,"data":[5]}
```

Write register address `0x05`, issue a repeated start, and read two bytes:

```json
{"type":"i2c.write_read","bus":0,"address":24,"write":[5],"readLength":2}
```

I2C addresses may also be sent as quoted hexadecimal strings such as
`"address":"0x18"`. Read and write payloads are limited to 64 bytes.

Example:

- `examples/i2c_ws.py`: Python status and bus scan client

## Known Limitations

- Current board profiles use DHCP and do not expose the older IPv4 link-local
  mode. Full AutoIP/RFC 3927 probing and ARP defense are not implemented.
- The DHCP mode does not probe the UID-derived `10.x.y.0/24` subnet before
  using it. A subnet collision is unlikely but possible in principle.
- The WebSocket protocol is intentionally small and JSON-only at the moment.
- The SCPI server currently supports one TCP client at a time and scalar channel
  numbers rather than SCPI channel-list expressions such as `(@1:4)`.
- RP2040 ADC voltage and temperature results are nominal, not calibrated
  instrument-grade measurements.
- The Embassy RP2040 I2C driver has no address-only probe operation. `i2c.scan`
  therefore probes each address with a one-byte read, which may affect devices
  whose reads have side effects.
- I2C transactions currently have no automatic stuck-bus recovery.
- There is no flash filesystem or custom page upload support in this Rust
  version yet. The firmware serves the built-in consoles selected at build time.

## Notes

The stable MAC and IP derivation makes repeated development sessions easier and
avoids all boards presenting the same hard-coded MAC address. The mDNS responder
also announces an IPv6 link-local AAAA record, which often gives macOS and iOS a
more reliable scoped route to the USB CDC-NCM interface.
