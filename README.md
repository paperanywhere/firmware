# paperanywhere-firmware

ESP32-S3 firmware for paperanywhere e-paper devices. Built with `esp-rs` no_std.

## Supported boards

Pick exactly one at build time via Cargo features:

| Feature | Board |
|---|---|
| `board-reterminal-e1001` (default) | Seeed Studio reTerminal E1001 (7.5" mono) |
| `board-reterminal-e1002` | Seeed Studio reTerminal E1002 (7.3" full color) |
| `board-reterminal-e1003` | Seeed Studio reTerminal E1003 (10.3" mono 16-gray) |
| `board-reterminal-e1004` | Seeed Studio reTerminal E1004 (13.3" full color) |
| `board-inkplate-6` | Soldered Inkplate 6 |
| `board-inkplate-10` | Soldered Inkplate 10 |
| `board-generic-esp32s3-waveshare-75` | Generic ESP32-S3 + Waveshare 7.5" BW |

## Setup

```bash
# One-time toolchain install
cargo install espup espflash
espup install

# Build for the default board (reTerminal E1001)
. ~/export-esp.sh      # Linux/macOS
. $env:USERPROFILE\export-esp.ps1  # Windows
cargo build --release

# Flash + monitor (requires the device on USB)
cargo run --release
```

## Pin maps

GPIO assignments in `src/boards/*.rs` are placeholders. Before flashing real
hardware, replace them with the values from the schematic for that board:

- reTerminal E-series: https://wiki.seeedstudio.com/reterminal_e10xx_main_page/
- Waveshare panels: the panel's wiki page
- Inkplate boards: https://github.com/SolderedElectronics/Inkplate-Arduino-library

## Provisioning paths

The firmware tries setup paths in priority order on every boot:

1. **`prov` partition** — a `paperanywhere-prov.bin` flashed alongside the firmware. Generate from the dashboard or with `cargo run -p paperanywhere-provtool -- gen ...`, then flash with `espflash write-bin --address 0x12000 prov.bin`. **Migrated to NVS on first successful boot, then the partition is erased** — subsequent boots read creds from NVS, so this is a one-time bootstrap channel.
2. **SD card `wifi.conf`** — only on boards with `has_sd_card` (reTerminal E-series + Inkplate). Drop the file on the SD root. Same blob, friendly TOML form. Convenient when the user has a card reader but doesn't want to install `espflash`.
3. **Existing NVS state** — re-boots after a successful first-boot fall here. WiFi creds are persistent, so the prov partition can be erased safely.
4. **Captive portal** — last-resort interactive setup. Device hosts an AP named `paperanywhere-XXXX`; phone connects, enters home WiFi creds in a captive page, device migrates them into NVS.

The dashboard wizard surfaces all four as explicit choices when creating a device.

### Factory reset

Long-press the primary button at boot (≥ 5 seconds) to wipe NVS. The device falls back to step 1 on the next cold boot — flash a new prov bundle, drop a new SD config, or use captive portal.

### Threat model + what this firmware does (and doesn't) do

This firmware ships with **plaintext flash**. Physical access to a stolen device
gives the attacker WiFi credentials and the device token. We mitigate exposure
by erasing the `prov` partition after the first successful boot — the
provisioning bundle is a one-time bootstrap channel, not the persistent store.

We do **not** burn eFuses, configure flash encryption, or enable secure boot.
Those operations are *permanent* on ESP32 hardware — once an eFuse bit is
written you cannot rewrite it, and a misconfiguration bricks the device for
that configuration forever. If you have a stronger threat model than "trusted
physical environment", the Espressif documentation covers those features, but
**enabling them is your decision and your action** — paperanywhere never
performs irreversible hardware operations on your behalf.

## Wire types

`paperanywhere-proto` is consumed as a git dependency on the **public**
[paperanywhere/proto](https://github.com/paperanywhere/proto) repo — same crate the backend
consumes, so the wire format never diverges. For sibling-checkout dev:

```toml
paperanywhere-proto = { path = "../paperanywhere-proto", default-features = false }
```

Tag releases of `paperanywhere-proto` are the canonical schema boundary; both
this firmware and the backend should pin to the same git rev (or version
once published to crates.io) for any production build.
