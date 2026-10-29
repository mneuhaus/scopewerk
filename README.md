# Scopewerk

The Hanmatek DOS1102 oscilloscope on your Mac: a native GPUI app with a live screen, every
control, measurements, cursors and export, plus a CLI and an MCP server so agents can work with
the scope too. Pure Rust over USB (`nusb`): no Python, no libusb, no VISA.

- **App**: live traces with afterglow, square divisions like the real screen (15.2 × 8), hover
  readout, A/B cursors (Δt, 1/Δt, ΔV), automatic measurements, run/stop/single/autoset, all
  channel, trigger and acquisition settings, CSV export, screenshots of the scope's own display.
- **Agents**: `scopewerk mcp` exposes the scope as MCP tools. While the app is open, agents and
  the CLI work *through* it, so you watch every change live and see it in the activity log.
- **Logic analyzer**: decode CAN (CRC and ACK checked, bitrate and polarity detected, one wire or
  CH1−CH2 differential), I²C, SPI (clock + one data line) and UART (auto baud) on every frame,
  with a protocol lane under the traces and a table of decoded events.
- **Verified changes**: the firmware silently ignores values it does not like. Scopewerk reads
  every change back from the scope and reports what really happened, snapping to the scope's
  1-2-5 steps.

## Build and install

```sh
scripts/bundle-macos.sh             # dist/Scopewerk.app (app + CLI inside)
scripts/bundle-macos.sh --install   # also copy to /Applications
cargo build --release -p scopewerk-cli   # just the CLI: target/release/scopewerk
```

Connect the **square USB-B port on the back** of the scope (the USB-A port on the front is for
sticks). The app finds the scope on its own and reconnects when it is unplugged.

## The app

| Key | Action |
| --- | --- |
| `space` | run / stop |
| `s` | single shot |
| `⌘⇧A` | autoset |
| `←` `→` | timebase faster / slower |
| `↑` `↓` | V/div of the selected channel |
| `⇧↑` `⇧↓` | offset of the selected channel |
| `1` `2` | select channel; `⌘1` `⌘2` switch it on/off |
| `t` | trigger level to 50 % of the signal |
| `m` | screen (1 520 points) / memory (10 000 samples) |
| `g` | afterglow on/off |
| `p` | pause the live view |
| click, `⌥`-click, `esc` | cursor A, cursor B, clear |
| scroll | timebase (`⌥` V/div, `⇧` offset) |
| `⌘E` | export CSV |
| `⌘⇧S` | save the scope's display as PNG (`~/Pictures/Scopewerk`) |
| `⌘⇧C` | copy measurements |

Environment: `SCOPEWERK_DEMO=1` shows a simulated fault-tolerant CAN bus instead of a USB scope
(try the decoder without hardware), `SCOPEWERK_BACKGROUND=1` opens a non-activating window that
never takes keyboard focus (scripted screenshots), `SCOPEWERK_WINDOW=1600x1200` sets the size.

## Logic analyzer

The Decode panel (app), `scopewerk decode …` (CLI) and `decode_bus` (MCP) decode the
10 000-sample acquisition memory. With two channels the DOS1102 covers:

| Protocol | Wiring | Detected automatically |
| --- | --- | --- |
| CAN 2.0A/B | CAN_H, CAN_L or a transceiver RX pin on one channel, or CAN_H/CAN_L on CH1/CH2 (`ch1-ch2`) | bitrate, polarity, threshold; CRC-15, stuffing and ACK are checked |
| I²C | SCL + SDA | thresholds; START, repeated START, STOP, address + R/W, ACK/NACK |
| SPI | clock + one data line (no chip select: words split at clock pauses) | CPOL from the idle clock (CPHA 0 unless given) |
| UART | one line | baud (snapped to standard rates), 8N1 by default |

The scope samples 500 points per division, so the timebase sets the samples per bit. About ten
are plenty: timebase ≤ 50 / bitrate (200 µs/div for 125 kbit/s CAN, 2 ms/div window). Scopewerk
warns and offers the right timebase when there are too few. To catch a rare frame, trigger on the
bus (e.g. CAN_L, falling edge, level mid-swing, sweep Normal) and use single shot.

```sh
scopewerk decode can --input ch1-ch2               # differential, everything detected
scopewerk decode can --bitrate 125k --single 10    # arm and wait up to 10 s for a frame
scopewerk decode i2c --scl 1 --sda 2
scopewerk decode spi --clk 1 --data 2 --mode 0
scopewerk decode uart --baud 115200
```

## CLI

```sh
scopewerk status                       # identity, connection, every setting
scopewerk capture                      # measurements of what is on screen
scopewerk capture --memory --csv bus.csv --png bus.png
scopewerk set timebase=1ms ch1.scale=500mV ch1.offset=-2 trigger.level=1.2V trigger.edge=fall
scopewerk single --timeout 30 --csv event.csv
scopewerk watch 60 --log levels.jsonl  # levels twice a second
scopewerk screenshot scope.png
scopewerk measure 1                    # the scope's own measurements
scopewerk scpi ':CH1:SCALe?'           # raw SCPI
```

Add `--json` to any command for machine-readable output. `scopewerk set --help` lists every key.

## MCP

```sh
claude mcp add --scope user scopewerk -- /Applications/Scopewerk.app/Contents/MacOS/scopewerk mcp
```

| Tool | What it does |
| --- | --- |
| `scope_status` | identity, connection path, all settings incl. the visible voltage range |
| `capture_waveform` | measurements, warnings (clipping, poor V/div use), plot image, optional samples and CSV |
| `configure_scope` | any settings in one call, each read back and reported |
| `run_control` | run, stop, single, autoset |
| `single_shot` | arm, wait for the trigger, return the capture |
| `monitor_levels` | min/mean/max/frequency over time, e.g. while a device powers up |
| `scope_screenshot` | the scope's real display as PNG |
| `scope_measurements` | the scope's built-in measurements |
| `decode_bus` | logic analyzer: CAN, I²C, SPI, UART with detected parameters, events and plot |
| `send_scpi` | raw SCPI escape hatch |

## Protocol notes (DOS1102, firmware V1.5.0, USB `5345:1235`)

The scope is a USBTMC device speaking OWON's SDS SCPI dialect. Verified on the device:

- Screen data `:DATA:WAVE:SCREen:CHn?`: 1 520 little-endian `i16` points across 15.2
  divisions, behind a 4-byte length prefix. Header `:DATA:WAVE:SCREen:HEAD?` is JSON.
- **Scaling: 400 counts per division, offsets in 1/50 division (8 counts per offset pixel).**
  These match the scope's own `:MEAS` min/max/pk-pk exactly. The values 410 and 8.25 used by
  earlier wrappers read about 2–3 % off.
- The header's channel `SCALE` is input-referred: with a 10X probe the scope's measurements read
  ten times higher while `SCALE` stays the same, so Scopewerk multiplies by the probe factor.
- Memory data `:DATA:WAVE:DEPMem:CHn?`: 10 000 samples at the real sample rate (the screen is
  the slice from `SCREENOFFSET`). Its header has no trigger block.
- Screenshot `:DATA:WAVE:SCREen:BMP?`: 800 × 480 24-bit BMP (1.15 MB, about 1.6 s). The reply
  must be requested in one USBTMC transfer; afterwards the firmware queues a stray `"…->\n"`
  message that has to be read and dropped.
- Each data query takes about 250 ms inside the scope (USB itself is microseconds), so a live
  view of both channels runs at about 1.5 frames per second, one channel about 2.5.
- Settings: `:HORIzontal:SCALe 1.0ms` (a bare `1ms` is ignored), `:HORIzontal:OFFSet <div>`,
  `:CHn:SCALe 500mV`, `:CHn:OFFSet <div>`, `:CHn:COUPling DC|AC|GND`, `:CHn:PROBe 10X`,
  `:CHn:DISPlay ON|OFF`, `:CHn:INVerse ON|OFF`, `:TRIGger:SINGle:EDGE:SOURce|SLOPe|LEVel|COUPling`,
  `:TRIGger:SINGle:SWEep AUTO|NORMal|SINGle`, `:RUNning RUN|STOP`, `:ACQuire:MODE SAMPle|PEAK|AVERage`.
  Set commands answer with an empty message; queries end in `->\n`.
- A program killed mid-transfer leaves the pipes stalled. Scopewerk clears them (USBTMC
  INITIATE_CLEAR, endpoint halts, drain) when it opens the scope and, if that is not enough,
  resets the device on the bus.

Older firmware (`5345:1234`, raw bulk without USBTMC framing) is detected and handled the way
[danielphili/hanmatek_DOS1102_python_wrapper](https://github.com/danielphili/hanmatek_DOS1102_python_wrapper)
describes, but is untested here. Scopewerk is an independent implementation; protocol facts come
from that wrapper, OWON's SDS SCPI document, and measurements on the device.

## Layout

- `crates/scopewerk-core`: driver (`usb`, `scope`, `control`), header and waveform model,
  PNG output, and the broker (Unix socket in `~/Library/Application Support/Scopewerk`).
- `crates/scopewerk-cli`: the `scopewerk` binary, CLI and MCP server.
- `crates/scopewerk-app`: the GPUI app.

## License

MIT
