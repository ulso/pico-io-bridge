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
compiling CAN support or configuring CAN/SPI pins. The Adafruit Feather RP2040
USB Host profile additionally runs a directly connected USB host through the
RP2040 PIO blocks while the native USB controller remains the CDC-NCM device.

## Current Features

- USB CDC-NCM Ethernet interface
- Embedded DHCP server using a UID-derived `10.x.y.0/24` subnet
- Stable locally administered MAC addresses derived from the Pico flash UID
- Stable 16-character USB serial number derived from the full 64-bit flash UID
- IPv6 link-local address derived from the device MAC
- mDNS/DNS-SD advertisement for `_http._tcp` and `_scpi-raw._tcp`, plus
  `_usbserial._tcp` on the USB-host profile
- Predictable board-specific hostnames with a flash-UID fallback on conflict
- SCPI-RAW instrument server on TCP port 5025 using `microscpi`
- Four-channel 12-bit ADC measurements on A0-A3, plus internal temperature
- Explicit SCPI configuration and ranging support for VL53L4CD I2C sensors
- AMG8833 8x8 thermal array measurements over SCPI
- BME688 temperature, humidity, pressure, and gas resistance over SCPI
- BNO08x acceleration, gyro, magnetic field, and fused orientation over SCPI
- LC709203F battery voltage and state-of-charge measurements over SCPI
- PCT2075 external temperature measurements over SCPI
- Adafruit seesaw rotary encoder position, delta, and push-button state over SCPI
- Built-in browser CAN and I2C consoles
- WebSocket CAN API at `/can`
- WebSocket I2C API at `/i2c`
- MCP25625 CAN support via the `mcp25xx` crate
- CAN TX, RX broadcast to connected WebSocket clients, status, bitrate/mode config
- Concurrent I2C status, scan, read, write, and write-read transactions
- PIO USB host with full-speed CDC-ACM and low-speed Velleman P8055 HID support
- Raw binary TCP bridge to a hosted CDC-ACM stream on port 7000
- SCPI-RAW bridge to a hosted USBTMC/USB488 instrument on port 5026
- Compile-time profiles for four Adafruit RP2040 boards
- Board status indication while the USB network is not ready, where available

## Hardware

Supported board profiles:

| Cargo feature | Interfaces | STEMMA QT | mDNS hostname |
| --- | --- | --- | --- |
| `board-adafruit-rp2040-can` | CAN, I2C, ADC | I2C1, SDA GP2, SCL GP3 | `pico-io-can-feather.local` |
| `board-adafruit-feather-rp2040` | I2C, ADC | I2C1, SDA GP2, SCL GP3 | `pico-io-feather.local` |
| `board-adafruit-rp2040-usb-host` | PIO USB host, I2C, ADC | I2C1, SDA GP2, SCL GP3 | `pico-io-usb-host.local` |
| `board-adafruit-kb2040` | I2C, ADC | I2C0, SDA GP12, SCL GP13 | `pico-io-kb2040.local` |

The regular Feather RP2040 and Feather RP2040 USB Host profiles select
Embassy's generic `03h` second-stage flash bootloader. Adafruit boards of these
models may contain either GD25Q64C or W25Q64JV flash, and the official Pico SDK
board definitions make the same generic compatibility choice. The KB2040 uses
the faster W25Q bootloader.

The PIO USB host profile has a fixed, compile-time resource contract:

| Resource | PIO USB host use |
| --- | --- |
| `clk_sys` | Exactly 120 MHz |
| PIO0 SM0 | USB transmit |
| PIO1 SM0/SM1 | USB receive and edge/EOP detection |
| DMA channel 0 | USB transmit DMA |
| GP16 / GP17 | D+ / D- |
| GP18 | Active-high enable for the board's current-limited 5 V VBUS switch |

Both PIO blocks remain reserved for the host firmware's lifetime. GP16 and GP17
are exact requirements of the pinned backend, not a configurable consecutive
pin pair. GP18 only controls the board's protected load switch and does not
source VBUS directly. The RP2040 native USB controller and `USBCTRL_IRQ` remain
dedicated to CDC-NCM, so device-side networking and the PIO root port operate
concurrently.

The PIO USB host manager runs on RP2040 core 1 together with PIO/DMA interrupt
handling, enumeration, pipe scheduling, and class transport. Core 0 retains
CDC-NCM networking, TCP, HTTP, SCPI, and the other application tasks. Bounded
`embassy-sync` channels carry commands and byte frames between the cores. This
separation is required to keep the CPU-assisted full-speed ACK turnaround free
from unrelated network and executor latency.

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

The CAN Feather, regular Feather RP2040, and Feather RP2040 USB Host use their
red LED on GP13 as a startup indicator. The existing `StatusIndicator` remains
the pin's sole owner; the PIO host manager reports its state through shared
state rather than taking the LED. The LED turns on after reset and turns off
when DHCP has assigned the CDC-NCM host an address and mDNS has established the
advertised services. The KB2040 profile cannot use GP13 as an LED because it is
the STEMMA QT clock pin. Its onboard NeoPixel is deliberately left untouched
for now, so network readiness is reported through the UART log instead.

If the host remains silent during startup, the firmware briefly disconnects
USB before retrying enumeration. The disconnect grows from 350 ms to 1, 3,
and finally 5 seconds so a host reconnecting an entire dock has time to discard
stale CDC-NCM state. The retry counter survives automatic recovery resets but a
reset-button press or a real link-down starts a fresh sequence. If the LED stays
red after the retry limit, physically unplug the board's USB cable for at least
five seconds; a bus-powered device cannot remove VBUS from its own hub port.

## Build

The project is configured for `thumbv6m-none-eabi` in `.cargo/config.toml`.
The root release profile uses `opt-level = "s"`, fat LTO, and one codegen unit.
These settings are intentionally repeated here because Cargo does not inherit a
dependency crate's release profile.

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

For the Adafruit Feather RP2040 USB Host:

```sh
cargo build --release --no-default-features --features board-adafruit-rp2040-usb-host
```

The three non-CAN profiles exclude the `mcp25xx` dependency, CAN task, CAN HTTP
page and `/can` WebSocket endpoint from the resulting firmware. The USB-host
profile adds
[`embassy-rp-pio-usb-host`](https://github.com/ulso/embassy-rp-pio-usb-host)
as an optional Git dependency pinned to commit
`d0481d2f9dc5169b2282701bf9e1fffec0be7534`.

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
http://pico-io-usb-host.local/usb-host.html USB host status
```

Only pages and WebSocket endpoints for interfaces in the selected board profile
are available. The CAN Feather uses the CAN console at `/`; the three non-CAN
profiles serve the I2C console there. The SCPI information page is available
in every profile. The read-only USB host page is available only in the
`board-adafruit-rp2040-usb-host` profile. It reports the PIO host phase, attached
device identity, speed, address, transfer counters, cumulative errors and fixed
hardware resources. Its live data comes from `/api/usb-host/status`.
`/api/status` reports instrument identity, SCPI-RAW connection metadata, the
active interface names and the endpoint paths in its `pages` and `websockets`
fields.

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
| `SYST:RES:CAUS?` | Reset cause captured at boot |
| `SYST:USB:HOST:STAT?` | PIO USB host state and transfer counters; USB-host profile only |
| `SYST:USB:HOST:ENUM:DIAG?` | Enumeration attempts and most recent failure details; USB-host profile only |
| `SYST:USB:HOST:FTDI:BAUD <300-3000000>` | Set the active FTDI UART baud rate; rejected while TCP port 7000 is in use |
| `SYST:USB:HOST:FTDI:BAUD?` | Read the active FTDI UART baud rate |
| `SYST:USB:HOST:CDC:WRITE:HEX "<hex>"` | Write 1-64 bytes to the enumerated CDC-ACM stream; USB-host profile only |
| `SYST:USB:HOST:CDC:READ:HEX? <length>` | Read up to 1-64 CDC-ACM bytes and return uppercase hex; USB-host profile only |
| `SYST:USB:HOST:CDC:EXCH:HEX? "<hex>",<max-length>` | Atomically write and collect a 1-64 byte CDC-ACM response; USB-host profile only |
| `SYST:USB:HOST:P8055:INP?` | Fresh P8055 digital, analog, and counter input snapshot |
| `SYST:USB:HOST:P8055:OUTP <digital>,<a1>,<a2>` | Atomically set the P8055 output shadow |
| `SYST:USB:HOST:P8055:OUTP?` | Read the confirmed P8055 output shadow |
| `SYST:USB:HOST:P8055:COUN:RES <channel>` | Reset P8055 counter 1 or 2 |
| `SYST:USB:HOST:P8055:COUN:DEB <channel>,<microseconds>` | Set P8055 counter debounce |
| `SYST:USB:HOST:P8055:COUN:DEB? <channel>` | Read the quantized debounce setting |
| `SYST:I2C:DEV:CAT?` | Supported I2C device models |
| `SYST:I2C:DEV:ADD <slot>,"<model>",<address>` | Verify, initialize, and register a device |
| `SYST:I2C:DEV? <slot>` | Device configuration as `slot,model,bus,address` |
| `SYST:I2C:DEV:LIST?` | All configured devices, or `NONE` |
| `SYST:I2C:DEV:COUN?` | Number of configured devices |
| `SYST:I2C:DEV:DEL <slot>` | Stop and remove a configured device |
| `SYST:I2C:DEV:CLEAR` | Stop and remove all configured devices |
| `SENS:AVER:COUN <count>` | Set the global ADC averaging count (`1`-`256`) |
| `SENS:AVER:COUN?` | Read the global ADC averaging count |
| `SENS:BATT:CAP <slot>,<mAh>` | Configure LC709203F battery capacity |
| `SENS:BATT:CAP? <slot>` | Read configured battery capacity in mAh |
| `SENS:ENC:POS <slot>,<position>` | Set a seesaw rotary encoder position |
| `MEAS:ADC:RAW? <channel>` | Averaged 12-bit ADC code for channel 0-3 |
| `MEAS:VOLT:DC? <channel>` | Averaged nominal voltage for channel 0-3 |
| `MEAS:TEMP?` | Approximate RP2040 internal temperature in degrees Celsius |
| `MEAS:TEMP:EXT? <slot>` | PCT2075 or BME688 temperature in degrees Celsius |
| `MEAS:HUM? <slot>` | BME688 relative humidity in percent |
| `MEAS:PRES? <slot>` | BME688 pressure in pascals |
| `MEAS:GAS:RES? <slot>` | BME688 gas resistance in ohms |
| `MEAS:BATT:VOLT? <slot>` | LC709203F cell voltage in volts |
| `MEAS:BATT:SOC? <slot>` | LC709203F state of charge in percent |
| `MEAS:IMU:ACC? <slot>` | BNO08x acceleration as `x,y,z,accuracy` |
| `MEAS:IMU:GYR? <slot>` | BNO08x angular velocity as `x,y,z,accuracy` |
| `MEAS:IMU:MAGN? <slot>` | BNO08x magnetic field as `x,y,z,accuracy` |
| `MEAS:IMU:QUAT? <slot>` | BNO08x quaternion and accuracy |
| `MEAS:ENC:POS? <slot>` | Seesaw rotary encoder position |
| `MEAS:ENC:DELTA? <slot>` | Seesaw rotary encoder change since the previous encoder read |
| `MEAS:ENC:BUTTON? <slot>` | Seesaw push button state, `1` means pressed |
| `MEAS:DIST? <slot>` | Distance in meters from a configured ranging sensor |
| `MEAS:THERM:PIX? <slot>,<pixel>` | AMG8833 pixel 0-63 in degrees Celsius |
| `MEAS:THERM:MIN? <slot>` | Minimum AMG8833 frame temperature |
| `MEAS:THERM:MAX? <slot>` | Maximum AMG8833 frame temperature |
| `MEAS:THERM:AVER? <slot>` | Mean AMG8833 frame temperature |
| `READ:ENV? <slot>` | BME688 temperature, humidity, pressure, and gas resistance |
| `READ:IMU? <slot>` | Complete BNO08x motion and orientation measurement |
| `READ:THERM:ARR? <slot>` | All 64 AMG8833 pixels in degrees Celsius |

SCPI channel 0 maps to A0/GP26, through channel 3 at A3/GP29. Voltage conversion
assumes a nominal 3.3 V ADC reference and is not calibrated. Keep analog inputs
between ground and 3.3 V; RP2040 GPIO pins are not 5 V tolerant.

On the USB-host profile, `SYST:USB:HOST:STAT?` returns:

```text
phase,speed,address,vid,pid,rx_bytes,tx_bytes,error_count,max_transfer,unexpected_toggle_count,accepted_zlp_count,latest_expected_pid,latest_actual_pid,latest_payload_len,latest_prefix_hex
```

For example, `CDC_READY,FULL,1,2458,1,12,4,0,64,0,0,NONE,NONE,NONE,NONE`
reports a configured full-speed CDC-ACM function, while
`P8055_READY,LOW,1,4303,21760,8,8,0,8,0,0,NONE,NONE,NONE,NONE` reports an
original P8055. The final six fields are a roughly 100 ms snapshot of accepted
zero-length IN packets and CRC-valid packets discarded because their DATA
toggle was unexpected. PIDs and the retained payload prefix are uppercase hex;
the prefix is at most eight bytes, `EMPTY` for a zero-length packet, and all
four latest-packet fields are `NONE` until one has been observed. The possible
phases are `POWER_OFF`, `WAITING`, `RESETTING`, `ENUMERATING`, `CDC_READY`,
`P8055_READY`, `UNSUPPORTED_SPEED`, `UNSUPPORTED_DEVICE`,
`ENUMERATION_ERROR`, `CDC_ERROR`, and `P8055_ERROR`. Speed is `FULL`, `LOW`, or
`NONE`.

`SYST:USB:HOST:ENUM:DIAG?` returns:

```text
attempts,failures,origin,error,site,handshake,setup_attempts,bmRequestType,bRequest,wValueLo,wValueHi,wIndexLo,wIndexHi,wLengthLo,wLengthHi
```

FTDI devices start at 115200 baud. The rate can be changed while the device is
ready and TCP port 7000 is idle. The selected rate is reused across FTDI
disconnects until the bridge firmware restarts. For example, the DLP-IOR4
requires 9600 baud:

```text
SYST:USB:HOST:FTDI:BAUD 9600
SYST:USB:HOST:FTDI:BAUD?
```

The DLP-IOR4 example configures 9600 baud over SCPI before using the raw TCP
bridge:

```sh
python3 examples/dlp_ior4_tcp.py ping
python3 examples/dlp_ior4_tcp.py set 1 A
python3 examples/dlp_ior4_tcp.py cycle 1 --delay 2
```

The relay contacts can switch hazardous voltages. Develop and test with
disconnected contacts or a safe low-voltage continuity circuit.

The diagnostic survives a successful automatic retry, making intermittent
enumeration failures visible after the device reaches a ready phase. Fields
without an observed failure are reported as `NONE`; setup bytes are uppercase
hex.

CDC transfers use owned 64-byte messages between SCPI and the host-manager task;
the manager remains the sole owner of enumeration state and all control,
bulk-IN, and bulk-OUT pipes. Hex input must contain an even number of
hexadecimal digits. The manager asserts DTR and RTS, then selects 115200 baud,
8 data bits, no parity, and one stop bit when it opens a CDC-ACM function. For
example, this atomically writes `AT` followed by CR and collects up to 64
response bytes without returning through SCPI between bulk-OUT and bulk-IN:

```text
SYST:USB:HOST:CDC:EXCH:HEX? "41540D",64
```

The exchange query returns uppercase hex without separators. It waits up to ten
seconds for the first non-empty response packet, then accumulates further
packets until the requested maximum is full or the stream is idle for 50 ms.
If the result exactly fills that maximum, more stream data may remain for a
subsequent raw read.
The separate write and read commands remain available for raw stream access;
their transfers have a two-second deadline, and the write command returns the
byte count. Data commands report a settings conflict unless the phase is
`CDC_READY`. A timed-out write has an indeterminate device-side byte count and
must not be retried blindly, because the device may already have accepted part
or all of it.

A standard-library-only Python example performs the status, atomic exchange,
error, hex-decoding, and text-decoding steps:

```sh
python3 examples/scpi_usb_host_cdc.py
python3 examples/scpi_usb_host_cdc.py ATI
```

### Raw TCP USB-serial bridge

The USB-host profile also exposes an enumerated CDC-ACM or FTDI UART byte
stream directly on TCP port 7000, advertised through DNS-SD as
`_usbserial._tcp`. This is a raw binary TCP bridge: it adds no banner,
framing, character encoding, or line ending, and it does not interpret `0xff`
or any other byte. It is not a Telnet or RFC 2217 server, so clients must not
send Telnet option negotiation.

One TCP client at a time owns the serial stream. The host manager remains the sole
owner of the USB control and bulk pipes and applies bounded backpressure
between TCP and USB. While a raw client holds the stream, the SCPI
`CDC:WRITE:HEX`, `CDC:READ:HEX?`, and `CDC:EXCHANGE:HEX?` data commands report
`-221,"Settings conflict"`; other SCPI commands, including the host-status
query, remain available. Closing the TCP connection releases the stream, and
detaching the USB device closes the active TCP connection. A new client can
connect after the same USB serial device is reattached without rebooting the
bridge.

CDC-ACM and FTDI's vendor-specific UART protocol are supported behind port
7000. Both are opened at a fixed 115200 baud, 8 data bits, no parity, and one
stop bit, with DTR and RTS asserted. The first FTDI hardware target is
FT232R/FT245R (`0403:6001`); the reusable class also recognizes FT2232,
FT4232H, FT232H, and FT230X default product IDs.

For the first FTDI acceptance test, connect TXD directly to RXD on the TTL
breakout and run:

```sh
python3 examples/ftdi_loopback_tcp.py
```

The default test keeps one TCP session open for 100 exchanges of 257
pseudorandom bytes. This crosses both the FTDI 62-byte serial-payload boundary
and the bridge's TCP/USB framing repeatedly, and compares every returned byte.

The standard-library-only Python client sends `AT` followed by CR by default,
matching BleuIO's command terminator. Use `--terminator crlf` for serial
devices that require CRLF.
For that exact probe it reassembles arbitrary USB/TCP fragments until the
echoed `AT` and a complete CRLF-delimited BleuIO `OK` or `ERROR` line arrive:

```sh
python3 examples/usb_serial_tcp.py
```

The packet-independent `AT` framing assumes BleuIO command echo is enabled, as
it is by default. With echo disabled (`ATE0`), select idle-delimited collection
explicitly:

```sh
python3 examples/usb_serial_tcp.py --idle-response
```

Other text commands use idle-delimited collection because `OK` is not a
universal BleuIO end marker; some commands have no final `OK`, while others
produce meaningful data after one. The idle timeout defaults to two seconds and
is configurable:

```sh
python3 examples/usb_serial_tcp.py ATI
python3 examples/usb_serial_tcp.py --idle-timeout 5 ATI
```

This response handling belongs only to the example client; the firmware bridge
remains a raw byte stream. For binary protocols, `--hex` sends exactly the
supplied bytes, never appends a terminator, and uses the same idle-delimited
collection:

```sh
python3 examples/usb_serial_tcp.py --hex "00 FF 0D 0A"
```

A native iOS or iPadOS app can use an ordinary TCP connection to port 7000 over
the bridge's CDC-NCM network. This path does not use Apple's External Accessory
framework or an MFi serial accessory; the app may still need iOS local-network
permission.

### Velleman K8055/P8055 over SCPI

The USB-host profile recognizes original Velleman boards with VID `0x10cf` and
PID `0x5500` through `0x5503`. They use low-speed HID interrupt transfers with
fixed eight-byte reports, rather than the CDC bulk stream. The host manager
reads the HID report descriptor, sends the documented all-off reset report,
validates one input report, and then enters `P8055_READY`.

`SYST:USB:HOST:P8055:INP?` performs a fresh interrupt-IN transfer and returns:

```text
digital_inputs,analog_input_1,analog_input_2,counter_1,counter_2
```

Digital inputs I1-I5 occupy bits 0-4. The response preserves the original
board's electrical convention: an open input is `1` and a grounded input is
`0`. Analog inputs are unsigned 8-bit samples and the two counters are unsigned
16-bit values.

`OUTP` applies all eight digital outputs and both 8-bit analog/PWM outputs in
one report. Counter reset and debounce reports include the confirmed output
shadow so they do not disturb other outputs. Debounce accepts channel 1 or 2
and `0` through `7477875` microseconds. The board represents it as the nearest
`115 * raw^2` microseconds; the query returns that actual quantized setting and
fails with `-230,"Data corrupt or stale"` until the setting has explicitly been
written after the current attachment. As with other failed SCPI queries, no
result line is emitted; retrieve the queued error with `SYST:ERR?`.

The Python and Octave examples are read-only by default:

```sh
python3 examples/scpi_usb_host_p8055.py

octave --quiet --eval \
  'addpath("examples"); scpi_usb_host_p8055'
```

Both print input snapshots and the confirmed output shadow. A digital output
pulse is deliberately opt-in; it preserves both analog outputs and every other
digital output, then restores and verifies the original shadow:

```sh
python3 examples/scpi_usb_host_p8055.py --pulse-output 1

octave --quiet --eval \
  'addpath("examples"); scpi_usb_host_p8055("pico-io-usb-host.local",5,1,0.5)'
```

The Octave example requires the `instrument-control` package. Never retry a
timed-out output command blindly: the physical state may have changed even
though its acknowledgement was lost. The manager enters `P8055_ERROR` after an
output timeout; physically replug the P8055 before issuing another output
command.

Each RP2040 ADC measurement takes a fresh block of samples and returns its
rounded arithmetic mean. `SENS:AVER:COUN` controls the block size globally for
A0-A3 and the internal temperature sensor; it does not affect P8055 inputs. The
default is 16 samples; `*RST` restores that default. Larger values reduce
uncorrelated noise but increase measurement latency proportionally.

### Known I2C Devices

Known devices are configured explicitly because an I2C scan generally reveals
only addresses, not reliable model identities. Eight logical slots are
available. The configuration is kept in RAM, survives `*RST`, and is cleared
when the firmware restarts.

#### VL53L4CD

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

#### AMG8833

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

#### PCT2075

The NXP PCT2075 temperature sensor is supported at the Adafruit breakout's
default address `0x37` and at its strap-selectable alternate addresses:

```text
SYST:I2C:DEV:ADD 3,"PCT2075",#H37
MEAS:TEMP:EXT? 3
```

`DEV:ADD` wakes the sensor and verifies its PCT2075-specific sample-period
register. Temperatures are reported in degrees Celsius at the sensor's
0.125 degree resolution. `DEV:DEL` and `DEV:CLEAR` put the sensor into its
low-power shutdown mode.

#### BME688

The Bosch BME688 environmental sensor is supported at the Adafruit breakout's
default address `0x77` and alternate address `0x76`:

```text
SYST:I2C:DEV:ADD 5,"BME688",#H77
MEAS:TEMP:EXT? 5
MEAS:HUM? 5
MEAS:PRES? 5
MEAS:GAS:RES? 5
READ:ENV? 5
```

`DEV:ADD` verifies chip ID `0x61` and the BME688 high-gas variant, reads the
factory calibration coefficients, and configures temperature, humidity,
pressure, filtering, and a 300 C gas-heater target. It also performs and
discards one heater warm-up cycle. Each query starts a fresh forced-mode
measurement. `READ:ENV?` is more efficient when all four values are needed
because it returns one coherent sample as
`temperature_C,humidity_percent,pressure_Pa,gas_resistance_ohm`.

Gas resistance is a raw compensated resistance, not an IAQ or gas
classification result. The heater can also make the reported temperature
slightly warmer than the surrounding air. Invalid measurements return SCPI NaN
values and queue a hardware error for `SYST:ERR?`. `DEV:DEL` and `DEV:CLEAR`
disable the gas heater and put the sensor into sleep mode.

#### BNO08x

The Adafruit BNO085 9-DoF Orientation IMU Fusion Breakout is supported at its
default address `0x4A` and alternate address `0x4B` selected by pulling DI high:

```text
SYST:I2C:DEV:ADD 7,"BNO08X",#H4A
MEAS:IMU:ACC? 7
MEAS:IMU:GYR? 7
MEAS:IMU:MAGN? 7
MEAS:IMU:QUAT? 7
READ:IMU? 7
```

`DEV:ADD` performs an SHTP software reset, verifies the product ID response,
and enables calibrated acceleration, gyroscope, magnetic-field, and absolute
rotation-vector reports at 10 Hz. Only the STEMMA QT connection is required;
the breakout's interrupt and reset pins are not used.

Acceleration is reported in m/s2, angular velocity in rad/s, and magnetic field
in microteslas. Vector queries return `x,y,z,accuracy`. The quaternion query
returns `i,j,k,real,accuracy_radians,accuracy`. Accuracy is the BNO08x status
level: `0` unreliable, `1` low, `2` medium, and `3` high.

`READ:IMU?` returns one complete measurement in this order:
`accel_x,accel_y,accel_z,accel_accuracy,gyro_x,gyro_y,gyro_z,gyro_accuracy,`
`mag_x,mag_y,mag_z,mag_accuracy,quat_i,quat_j,quat_k,quat_real,`
`quat_accuracy_radians,quat_accuracy`. `DEV:DEL` and `DEV:CLEAR` reset the
device and stop active reports.

#### LC709203F

The onsemi LC709203F battery monitor is supported at its fixed address `0x0B`.
The monitor must have a sufficiently charged single-cell LiPo or LiIon battery
connected because the IC is powered by the battery rather than STEMMA QT VIN:

```text
SYST:I2C:DEV:ADD 4,"LC709203F",#H0B
SENS:BATT:CAP 4,500
SENS:BATT:CAP? 4
MEAS:BATT:VOLT? 4
MEAS:BATT:SOC? 4
```

`DEV:ADD` verifies IC version `0x2717` and validates the response CRC before
waking the monitor. `SENS:BATT:CAP` accepts `100`, `200`, `500`, `1000`, `2000`,
or `3000` mAh, writes the corresponding APA value, selects the 4.2 V battery
profile, and restarts the state-of-charge calculation. Voltage can be measured
before capacity configuration; SOC returns a settings-conflict error until a
capacity has been selected. Every register transaction uses the LC709203F CRC-8.
`DEV:DEL` and `DEV:CLEAR` put the monitor into sleep mode.

#### Adafruit seesaw Rotary Encoder

The Adafruit I2C QT seesaw Rotary Encoder is supported at its default address
`0x36` and strap-selectable alternate addresses through `0x3D`:

```text
SYST:I2C:DEV:ADD 6,"SEESAW_ENCODER",#H36
MEAS:ENC:POS? 6
MEAS:ENC:DELTA? 6
MEAS:ENC:BUTTON? 6
SENS:ENC:POS 6,0
```

`DEV:ADD` resets the seesaw coprocessor, verifies that its encoder module is
present, configures the breakout's GPIO 24 push button with a pull-up, and
clears any initial encoder delta. Position and delta are signed 32-bit counts.
The button query returns `1` while pressed and `0` while released.
Reading either position or delta clears the seesaw firmware's accumulated
delta. Query delta before position when both values are needed in the same
polling cycle. `SENS:ENC:POS` can set or zero the absolute position. `DEV:DEL`
and `DEV:CLEAR` disable the button pull-up.

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
- `examples/scpi_common.py`: shared interactive board selector for PyVISA examples
- `examples/scpi_amg8833.py`: PyVISA AMG8833 8x8 thermal frame reader
- `examples/scpi_bme688.py`: PyVISA BME688 environmental measurement
- `examples/scpi_bno08x.py`: PyVISA BNO08x motion and orientation measurement
- `examples/scpi_lc709203f.py`: PyVISA LC709203F battery monitor
- `examples/scpi_pct2075.py`: PyVISA PCT2075 temperature measurement
- `examples/scpi_seesaw_encoder.py`: PyVISA seesaw rotary encoder and button reader
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
- The PIO host supports one directly connected root device and no hubs,
  high-speed devices, or isochronous transfers. The integration exposes generic
  full-speed CDC-ACM and the original low-speed Velleman K8055/P8055 protocol;
  it is not yet a general-purpose HID instrument API.
- The raw USB-serial TCP bridge accepts one client, supports CDC-ACM only, and
  uses fixed line settings; it does not implement Telnet or RFC 2217 controls.
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
### USBTMC instruments over Ethernet

The USB-host profile recognizes full-speed USBTMC interfaces (`FE/03`) and
their USB488 extension (`FE/03/01`). Instrument SCPI is exposed separately
from the bridge's own command tree:

- TCP 5025 controls Pico I/O Bridge itself.
- TCP 5026 forwards SCPI program messages to the attached USBTMC instrument.

The USBTMC socket is advertised as `_usbtmc._tcp`. For example, query a
Keysight 34450A connected to the host port with:

```sh
printf '*IDN?\n' | nc -w 10 pico-io-usb-host.local 5026
python3 examples/usbtmc_tcp.py
```

The initial implementation supports bounded textual program messages and
responses up to 512 bytes. USB488 interrupt status/SRQ, abort/clear recovery,
binary blocks, and larger streaming responses are future extensions.
