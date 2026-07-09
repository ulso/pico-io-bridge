# Pico CAN Bridge RS

Rust firmware for a Raspberry Pi Pico / RP2040 that exposes a CAN bus over USB
CDC-NCM. The board appears as a small USB Ethernet device, advertises itself as
`pico-can-bridge.local`, serves a minimal browser UI, and accepts CAN commands
over WebSocket.

The current default build is intended for an MCP25xx CAN controller connected
over SPI.

## Current Features

- USB CDC-NCM Ethernet interface
- IPv4 link-local address derived from the Pico flash UID
- Optional embedded DHCP server using a UID-derived `10.x.y.0/24` subnet
- Stable locally administered MAC addresses derived from the Pico flash UID
- IPv6 link-local address derived from the device MAC
- mDNS/DNS-SD advertisement for `_http._tcp`
- Built-in browser CAN console
- WebSocket CAN API at `/can`
- MCP25xx CAN support via the `mcp25xx` crate
- CAN TX, RX broadcast to connected WebSocket clients, status, bitrate/mode config

## Hardware

Default CAN wiring:

| Signal | Pico pin |
| --- | --- |
| SPI1 SCK | GP14 |
| SPI1 MOSI | GP15 |
| SPI1 MISO | GP8 |
| MCP25xx CS | GP19 |

The default CAN bitrate is 500 kbit/s.

## Build

The project is configured for `thumbv6m-none-eabi` in `.cargo/config.toml`.

```sh
cargo build --release
```

Default features are:

```text
mdns, mcp2515
```

For MCP25625 instead of MCP2515:

```sh
cargo build --release --no-default-features --features mdns,mcp25625
```

To try the embedded DHCP server:

```sh
cargo build --release --features dhcp-server
```

In this mode the Pico uses `10.x.y.1/24` and leases `10.x.y.2/24` to the USB
host. The `x.y` subnet bytes are derived from the flash UID, so different boards
should normally land on different private /24 networks. The DHCP response does
not advertise a router or DNS server; `.local` discovery still comes from mDNS.

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

The default build uses IPv4 link-local networking. With the optional
`dhcp-server` feature enabled, the device instead uses its UID-derived
`10.x.y.1/24` address and gives the host `10.x.y.2/24`.

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
http://pico-can-bridge.local/
```

## Local HTML Apps

Custom control pages do not have to be uploaded to the Pico. A standalone HTML
file on the host can connect directly to:

```text
ws://pico-can-bridge.local/can
```

For example, `examples/led_control.html` can be opened directly in Safari and
uses the WebSocket API to switch a CAN-connected LED node on and off. This keeps
the firmware simple while still allowing project-specific browser tools.

## WebSocket API

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

## Known Limitations

- There is no full AutoIP/RFC 3927 implementation yet.
- IPv4 link-local address selection is deterministic, based on flash UID plus a
  role salt. It does not currently perform ARP probing before claiming the
  address.
- ARP defense is not implemented yet. If another host uses the same IPv4
  link-local address, the firmware will not automatically move to a new address.
- The IPv4 address space used here is only the normal 169.254/16 link-local
  range, so collisions are possible in principle.
- The optional DHCP mode avoids the 169.254/16 link-local route ambiguity, but it
  still does not probe the chosen `10.x.y.0/24` subnet before using it.
- The WebSocket protocol is intentionally small and JSON-only at the moment.
- There is no flash filesystem or custom page upload support in this Rust
  version yet. The root page is always the built-in CAN console.

## Notes

The stable MAC and IP derivation makes repeated development sessions easier and
avoids all boards presenting the same hard-coded MAC address. The mDNS responder
also announces an IPv6 link-local AAAA record, which often gives macOS and iOS a
more reliable scoped route to the USB CDC-NCM interface.
