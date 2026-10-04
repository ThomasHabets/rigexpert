# RigExpert

A Rust library and Linux terminal application for the **RigExpert AA-650 ZOOM**
over Bluetooth Low Energy. The terminal UI uses ratatui; the CLI uses clap derive.
The default device is `04:91:62:AE:BA:ED`, override it with `--device`.

## Run

Rust 1.89 or newer, BlueZ, pkg-config, and the D-Bus development
library are required. On Debian/Ubuntu, install missing system prerequisites with:

```sh
sudo apt install bluez pkg-config libdbus-1-dev
sudo systemctl start bluetooth
```

These packages are already available on the development machine. No dependency
is vendored or patched.

```sh
cargo run --release -- --demo       # try the UI without hardware
cargo run --release                # connect to the AA-650 and open the UI
cargo run --release -- --help
cargo install --path .             # install the rigexpert executable locally
```

Power on the analyzer and enable Bluetooth on it. Close other applications using
its Bluetooth connection. This is BLE/GATT, so `/dev/rfcomm0` is not used. Pairing
is not performed automatically. The application discovers the analyzer through
BlueZ, then uses a direct ATT socket to answer its peer requests immediately.
It falls back to BlueZ's GATT API if the direct connection fails. Neither route
requires running Cargo as root. Starting the UI connects and reads identity; RF measurements begin
only when you press Space.

## Terminal controls

| Key | Action |
| --- | --- |
| Tab / Shift-Tab | Switch between Live, Sweeps, Smith, TDR, Cable, Memory |
| Space | Start measurement, or cancel the current operation |
| Shift+B | Choose an amateur band, with separate regional entries when limits differ |
| e | Edit measurement settings; in Cable, edit cable inputs |
| p | Toggle repeated acquisition |
| Up / Down, k / j | Select a local sweep, or a record in Memory; also navigate the band picker |
| Left / Right | Move frequency or TDR cursor; Sweeps shows a white vertical cursor line |
| Shift-Left / Shift-Right | Move the TDR cursor faster |
| + / - | Zoom around the selected cursor |
| m | Cycle SWR, R/X, return loss, impedance magnitude, phase; in TDR: impulse/step/impedance |
| b | Toggle comparison overlay, up to three plus the selected sweep |
| n | Rename selected sweep |
| s | Save the complete session as JSON |
| x | Export selected sweep as CSV, Touchstone `.s1p`, or JSON |
| l | Load a session, CSV, or Touchstone file |
| r | Reconnect without automatically resuming measurement |
| f | In Memory, refresh the record list |
| Enter | In Memory, download selected record |
| o / Shift+K | Mark selected sweep as open / short for cable analysis |
| a / d | In Cable, add / remove a cable section into a new sweep |
| g | In TDR or Cable, select the strongest reflection beyond one resolution cell |
| v | In Cable, estimate velocity factor from known length and TDR cursor |
| u | Switch TDR distance display between metres and feet |
| ? | Help |
| q / Ctrl-C | Stop, disconnect, restore the terminal, and exit |

Help groups shortcuts by task, with two columns on wide terminals. On smaller
terminals, use Up/Down or k/j and Page Up/Down to scroll; Esc closes help.

Press **Shift+B**, use arrows or Page Up/Down to choose a band, and press Enter
to set its sweep range. This switches to Sweeps and keeps your sample count and
reference impedance; **Space** starts the measurement. Esc cancels the picker.
The presets cover 2200 m through 70 cm within the AA-650's range, with shared
Region 1 and Region 2 entries when their limits match, and separate entries
when they differ. National limits may differ.
The picker shows the band limits and actual sweep limits: fractional-kHz edges
and odd-kHz spans round outward to the analyzer's whole-kHz center/span grid.

Presets follow the IARU [Region 1 HF](https://www.iaru-r1.org/wp-content/uploads/2021/06/hf_r1_bandplan.pdf),
[Region 1 VHF](https://www.iaru-r1.org/wp-content/uploads/2020/12/VHF-Bandplan.pdf),
[Region 1 UHF](https://www.iaru-r1.org/wp-content/uploads/2021/03/UHF-Bandplan.pdf), and
[Region 2](https://www.iaru-r2.org/wp-content/uploads/2020/02/IARU-Region-2-Band-plan.pdf)
band plans.

In dialogs, Tab or arrows select a field, Ctrl-U clears its contents, Enter
applies, and Esc cancels. Frequency fields accept `145.5MHz`, `100kHz`, or plain
Hz. Overwriting an existing file requires `y` confirmation. File operations run
outside the device task. For best visibility use a terminal of at least 80×24;
50×16 is the minimum layout.

The default sweep is **144–146 MHz, 201 samples, 50 Ω**. A sample count includes
both endpoints; the device command receives one fewer interval. BLE frequencies
must be whole kHz and the span an even number of kHz so the center frequency is
represented exactly. Limits come from the device and are bounded by the
AA-650's 100 kHz–650 MHz rating. Live mode acquires two readings at the same
frequency with zero span. Repeated live acquisitions replace the previous live
reading; ordinary sweeps remain in the session until another file is loaded.

SWR charts use a fixed, compressed 1–10 scale with marks at 1, 1.2, 1.5, 2,
3, 5, and 10, giving more detail near a good match. Values above 10 are clipped
at the top; smaller terminals show fewer labels on the same scale. Return loss
is capped at 100 dB. Cursor readouts show actual values, including infinity.
Raw sweeps retain their device metadata,
reference impedance, acquisition timestamp, and complete/partial status. A
cancelled or failed acquisition keeps the samples already received.

## Command line

```sh
rigexpert scan --seconds 5
rigexpert info
rigexpert measure 145.5MHz
rigexpert sweep --start 144MHz --stop 148MHz --samples 201 --z0 50
rigexpert sweep --output antenna.s1p
rigexpert sweep --repeat                  # Ctrl-C stops
rigexpert memory list
rigexpert memory download 0 --output stored.csv
rigexpert tui --load session.json
rigexpert --adapter hci0 --device 04:91:62:AE:BA:ED info
rigexpert --demo memory download 1 --output cable-open.json
```

`info` and `memory list` print JSON. Measurements print impedance CSV unless
`--output` is supplied. Files are never replaced without `--overwrite` on the
CLI. Repetition with a file requires `--overwrite`; each completed sweep replaces
that file. Incomplete acquisitions are exported with status metadata and produce
a nonzero exit status. CSV on stdout is measurement data only; use file export to
retain metadata. `--timeout` bounds discovery, connection, and identification
across up to three attempts, default 15 seconds.
A sweep has a 3-second inactivity timeout and a 120-second overall deadline.

## Analysis workflows

### TDR and cable length

Select TDR and press Space to acquire 100 kHz to the reported device maximum,
using up to 500 intervals. Set cable velocity factor in Cable (`e`), return to
TDR, and select the reflection with the cursor or `g`. Its distance estimates
cable length; for a known length, enter it in Cable and press `v` to estimate VF.
The demo's `Cable open` and `Cable short` records model a 10 m, VF 0.66 cable.

TDR is calculated locally from complex reflection data with a half Hamming
window, real estimated DC, Hermitian extension, zero padding, and normalized
inverse FFT. It requires a **complete, uniform, broadband** sweep with at least
16 samples and a start frequency no greater than one frequency step. The
missing DC value is estimated from the first reflection magnitude and sign.
The display shows the positive half of the transform period to avoid negative
time wrapping; its range is conservative. Distance uses the round-trip delay:
`distance = c * VF * time / 2`. Zero padding improves cursor spacing, not physical
resolution. The displayed resolution is approximately `c * VF / (2 * bandwidth)`.
Step-response impedance is an estimate; singular values are marked undefined.

### Cable loss and impedance

Measure the cable with an open far end, select that sweep and press `o`. Repeat
with a short and press `Shift+K`. The Cable tab estimates characteristic impedance
from `sqrt(Zopen * Zshort)` when both sweeps are complete and use the same grid.
Select either labelled sweep to show its one-way loss estimate at the cursor:
`loss_dB = -10 log10(|reflection|)`. These assume ideal terminations and a cable
matched to the configured cable impedance; this is a one-port estimate, not a
measured two-port insertion loss. User-labelled terminations are not detected by
the instrument.

### Cable reference plane and stubs

In Cable settings enter impedance, physical length, VF, conductor and dielectric
attenuation in dB/m at a reference frequency. Attenuation is modelled as
`conductor * sqrt(f/f_ref) + dielectric * (f/f_ref)`. `a` adds a uniform line in
front of the selected load; `d` removes it to estimate the far-end load. Each
operation creates a new sweep and preserves the original. Singular or
nonphysical results are rejected.

Enter target reactance to calculate the shortest nonnegative lossless open and
short stub lengths at the frequency cursor. The UI also reports interpolated
measured X=0 crossings. Calibration is managed **on the analyzer**; this software
uses returned R/X and does not implement host OSL correction. Device firmware
updates, screen/keypad emulation, and memory deletion are not implemented.

## Files

- `.json`: version 1 session with named sweeps, raw samples, status, device
  identity, timestamps, measurement settings, and cable settings.
- `.csv`: `frequency_hz,r_ohm,x_ohm`, with a RigExpert JSON metadata comment.
  Imports also accept this header without metadata, using the selected Z0.
- `.s1p`: Touchstone 1.0, one-port S11. Export uses Hz, real/imaginary, and the
  sweep's reference impedance. Import supports Hz/kHz/MHz/GHz and RI/MA/DB;
  absent option lines use the standard GHz/S/MA/50 Ω defaults. Touchstone 2.0,
  multiport, and impedance/admittance parameter files are rejected.

RigExpert metadata comments preserve incomplete status on round trips. Other
programs may discard comments. File writes use a temporary sibling file and
atomic replacement; input is bounded at 64 MiB. Import/export does not require
Bluetooth; the UI can load files while disconnected or in demo mode.

## Library

See [examples/sweep.rs](examples/sweep.rs) for a runnable client:

```rust,no_run
use rigexpert::{Analyzer, ConnectionOptions, SweepSettings};
use tokio_util::sync::CancellationToken;

# async fn example() -> rigexpert::Result<()> {
let mut analyzer = Analyzer::connect(&ConnectionOptions::default()).await?;
let result = analyzer.sweep(
    SweepSettings::default(),
    &CancellationToken::new(),
    |progress| println!("{progress:?}"),
).await;
let cleanup = analyzer.disconnect().await;
let sweep = result?;
cleanup?;
println!("{:?}", sweep.status);
# Ok(())
# }
```

All public library frequencies are Hz, impedances ohms, and distances metres.
`Analyzer` exclusively owns a `Transport`; operations take `&mut self` to prevent
command overlap. `Analyzer::with_transport` accepts a custom transport, and
`Analyzer::demo` runs the binary protocol against the simulator. Progress callbacks
should be short and nonblocking. Bluetooth callers should explicitly disconnect
before dropping their Tokio runtime; transport-drop cleanup is best effort.
Legacy zero-span readings are assembled in arrival order because the trailing
counter is undocumented and ignored by AntScope; identical retransmissions
cannot be distinguished from repeated readings in that mode.
The TUI maintains idle heartbeats. Library users with idle sessions should call
`ping()` roughly every two seconds. Each acquisition synchronizes with a ping,
validates notification CRCs, and stops with BREAK on completion or cancellation.

The independently implemented wire layouts are based on RigExpert's public
[AntScope2 BLE source](https://github.com/rigexpert/AntScope2/blob/master/analyzer/ble_analyzer.cpp)
and [protocol constants](https://github.com/rigexpert/AntScope2/blob/master/analyzer/ble_analyzer.h).
Both the legacy float R/X and newer packed R/X formats are supported. Presence
of the optional CRC-return characteristic selects packed mode. Zero packed pairs
are treated as unused entries, matching the manufacturer's decoder. A genuine
all-zero packed R/X pair therefore cannot be distinguished from padding and will
leave that acquisition incomplete.

## Bluetooth troubleshooting

The client scans using LE transport and the target address, without requiring
service UUIDs in advertisements. It reports which connection stage timed out
and disconnects failed attempts before returning, so retries do not leave the
analyzer occupied. The TUI automatically retries a lost or failed connection
after five seconds; `r` retries immediately. Reconnecting leaves measurements
stopped and reports Ready when identification succeeds.

A link-level `Connection Timeout (0x08)` in `btmon` differs from an application
command timeout. On the development adapter, the initial supervision timeout
was only 420 ms. A longer supervision timeout may help tolerate brief radio
interruptions, but it does not fix weak signal or interference. Linux caches
connection parameters per device, so changing the adapter default can leave an
existing device's timeout unchanged. Verify the actual timeout in a fresh
`LE Enhanced Connection Complete` event. The kernel's
[connection setup](https://github.com/torvalds/linux/blob/master/net/bluetooth/hci_sync.c)
uses cached device parameters before adapter defaults; a privileged user can
update a specific peer with BlueZ's
[Load Connection Parameters command](https://github.com/bluez/bluez/blob/5.82/doc/mgmt-api.txt).
For this analyzer on hci0, the helper sets a four-second timeout for the peer
without changing other devices or restarting Bluetooth:

```sh
sudo python3 extra/set-ble-parameters.py
```

It needs root for the management socket. The setting is temporary; reapply it
before connecting if BlueZ has discarded the unpaired device's cached settings,
or after rebooting or resetting the adapter. It retains the 30–50 ms connection
interval and zero peripheral latency.

For a persistent adapter default, set the following in `/etc/bluetooth/main.conf`:

```ini
[LE]
ConnectionSupervisionTimeout = 400
```

The value uses 10 ms units, so 400 means four seconds. This applies to new LE
connections on the adapter, including devices other than the analyzer. BlueZ
loads it at startup; cached per-device parameters can still override it. Keep a
backup of the original configuration before changing it.

Try these commands from your normal account with the analyzer powered on:

```sh
bluetoothctl
```

Then, inside its interactive prompt:

```text
power on
scan on
connect 04:91:62:AE:BA:ED
info 04:91:62:AE:BA:ED
menu gatt
list-attributes 04:91:62:AE:BA:ED
back
scan off
quit
```

The RigExpert service UUID should be `d973f2e0-b19e-11e2-9e96-0800200c9a66`.
Expected notification and command characteristics are
`706e4f15-3ee6-41c6-ba10-ca8abdcf3043` and
`6f8963a8-21e9-4055-86b8-2f911d736cff`. If absent, verify analyzer Bluetooth
settings and close competing clients. If connection requires authentication,
use `agent on`, `default-agent`, and `pair 04:91:62:AE:BA:ED` in bluetoothctl.

Only if BlueZ reports an access/authorization denial, run the same diagnostic
prompt with `sudo bluetoothctl` from a more privileged account and share the
error and service list. Do not change broad D-Bus permissions or run Cargo as
root. A powered-off adapter needs `bluetoothctl power on`; a stopped daemon
needs `sudo systemctl start bluetooth`.

If the link connects briefly but GATT services never resolve, capture the
daemon's diagnosis from a privileged account:

```sh
sudo systemctl kill --kill-whom=main --signal=SIGUSR2 bluetooth
sudo journalctl -u bluetooth -f -o cat
```

Run `cargo run -- info` from your normal account while logging. SIGUSR2 toggles
BlueZ debug logging; send it again afterward to turn logging off. On this
AA-650 with BlueZ 5.82, the log showed `Received request while another is
pending: 0x06` immediately after a peer MTU request. BlueZ then closed the
ATT channel before exposing any services. That failure occurs in the daemon
before RigExpert commands can be sent; changing crate command timeouts does
not resolve it. The application's direct ATT transport avoids that rejection
and uses the standard MTU of 23 for its 20-byte RigExpert packets. Leave the
system `ExchangeMTU` setting at its default: setting it to 23 prevented adapter
registration on the development machine because the kernel rejected the
daemon's listening socket configuration.

## Validation

```sh
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
cargo run --example sweep -- --demo
```

Tests cover wire fixtures, CRCs, both measurement encodings, duplicate/missing
samples, cancellation, timeout, disconnect, memory downloads, known RF loads,
10 m synthetic TDR, transmission-line round trips, file round trips, CLI
validation, TUI controls, layouts, and worker lifecycle.

Hardware TUI connection and clean exit, identity, reconnection, zero-span measurement, and an 11-point sweep
from 144 to 148 MHz were validated on `04:91:62:AE:BA:ED`
(serial `165001190`, firmware `1.5.1`) using the direct ATT transport from the
normal development account. Device memory and repeated acquisition still need
separate hardware validation.
