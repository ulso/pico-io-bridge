# Pico CAN and I2C Bridge RS

Rust firmware for an Adafruit RP2040 CAN Bus Feather that exposes its CAN bus
and STEMMA QT I2C bus over USB CDC-NCM. The board appears as a small USB
Ethernet device, advertises itself as `pico-can-bridge.local`, serves browser
consoles, and accepts bus commands over WebSocket.

The current default build is intended for an MCP25xx CAN controller connected
over SPI.

## Current Features

- USB CDC-NCM Ethernet interface
- Embedded DHCP server using a UID-derived `10.x.y.0/24` subnet
- Optional IPv4 link-local fallback derived from the Pico flash UID
- Stable locally administered MAC addresses derived from the Pico flash UID
- IPv6 link-local address derived from the device MAC
- mDNS/DNS-SD advertisement for `_http._tcp`
- Built-in browser CAN and I2C consoles
- WebSocket CAN API at `/can`
- WebSocket I2C API at `/i2c`
- MCP25xx CAN support via the `mcp25xx` crate
- CAN TX, RX broadcast to connected WebSocket clients, status, bitrate/mode config
- Concurrent I2C status, scan, read, write, and write-read transactions
- Board LED startup indicator while the USB network is not ready yet

## Hardware

Default CAN wiring:

| Signal | Pico pin |
| --- | --- |
| SPI1 SCK | GP14 |
| SPI1 MOSI | GP15 |
| SPI1 MISO | GP8 |
| MCP25xx CS | GP19 |

The default CAN bitrate is 500 kbit/s.

STEMMA QT wiring is fixed by the board:

| Signal | RP2040 pin |
| --- | --- |
| I2C1 SDA | GP2 |
| I2C1 SCL | GP3 |

The I2C bus runs at 400 kHz. CAN uses SPI1 and STEMMA QT uses I2C1, so both
hardware blocks run concurrently without a pin conflict.

The firmware uses the board's red LED on GP13 as a startup indicator.
The LED turns on after reset and turns off when DHCP has assigned the host an
address and mDNS has established the advertised service. In the non-DHCP
link-local configuration, seeing host traffic replaces the DHCP requirement.
If the host remains silent during startup, the firmware briefly disconnects
USB before retrying enumeration. The retry counter survives automatic recovery
resets but a reset-button press or a real link-down starts a fresh sequence.

## Build

The project is configured for `thumbv6m-none-eabi` in `.cargo/config.toml`.

```sh
cargo build --release
```

Default features are:

```text
mdns, mcp2515, dhcp-server
```

With the default features the Pico uses `10.x.y.1/24` and leases
`10.x.y.2/24` to the USB host. The `x.y` subnet bytes are derived from the
flash UID, so different boards should normally land on different private /24
networks. The DHCP response does not advertise a router or DNS server; `.local`
discovery still comes from mDNS.

For MCP25625 instead of MCP2515:

```sh
cargo build --release --no-default-features --features mdns,mcp25625,dhcp-server
```

To build the older IPv4 link-local mode without DHCP:

```sh
cargo build --release --no-default-features --features mdns,mcp2515
```

## Flash

The cargo runner is configured for `elf2uf2-rs`:

```toml
runner = "elf2uf2-rs --deploy --serial --verbose"
```

With the Pico in BOOTSEL mode, this should usually be enough:

```sh
cargo run --release
```

## Network Use

The default build uses the embedded DHCP server. The device uses its UID-derived
`10.x.y.1/24` address and gives the host `10.x.y.2/24`. IPv4 link-local
networking is still available by building without the `dhcp-server` feature.

The device advertises:

```text
pico-can-bridge.local
_http._tcp
```

Useful checks on macOS:

```sh
dns-sd -B _http._tcp
dns-sd -G v4v6 pico-can-bridge.local
ping pico-can-bridge.local
ping6 pico-can-bridge.local
curl http://pico-can-bridge.local/api/status
```

The built-in web UI is available at:

```text
http://pico-can-bridge.local/          CAN console
http://pico-can-bridge.local/i2c.html I2C console
```

## Local HTML Apps

Custom control pages do not have to be uploaded to the Pico. A standalone HTML
file on the host can connect directly to:

```text
ws://pico-can-bridge.local/can
ws://pico-can-bridge.local/i2c
```

For example, `examples/led_control.html` can be opened directly in Safari and
uses the WebSocket API to switch a CAN-connected LED node on and off. This keeps
the firmware simple while still allowing project-specific browser tools.

## CAN WebSocket API

WebSocket endpoint:

```text
ws://pico-can-bridge.local/can
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

## I2C WebSocket API

WebSocket endpoint:

```text
ws://pico-can-bridge.local/i2c
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

- There is no full AutoIP/RFC 3927 implementation for the non-DHCP link-local
  fallback yet.
- IPv4 link-local address selection is deterministic, based on flash UID plus a
  role salt. It does not currently perform ARP probing before claiming the
  address.
- ARP defense is not implemented yet. If another host uses the same IPv4
  link-local address, the firmware will not automatically move to a new address.
- The IPv4 address space used here is only the normal 169.254/16 link-local
  range, so collisions are possible in principle.
- The default DHCP mode avoids the 169.254/16 link-local route ambiguity, but it
  still does not probe the chosen `10.x.y.0/24` subnet before using it.
- The WebSocket protocol is intentionally small and JSON-only at the moment.
- The Embassy RP2040 I2C driver has no address-only probe operation. `i2c.scan`
  therefore probes each address with a one-byte read, which may affect devices
  whose reads have side effects.
- I2C transactions currently have no automatic stuck-bus recovery.
- There is no flash filesystem or custom page upload support in this Rust
  version yet. The firmware always serves its built-in CAN and I2C consoles.

## Notes

The stable MAC and IP derivation makes repeated development sessions easier and
avoids all boards presenting the same hard-coded MAC address. The mDNS responder
also announces an IPv6 link-local AAAA record, which often gives macOS and iOS a
more reliable scoped route to the USB CDC-NCM interface.
