# ToughFix desktop

The Rust application uses GTK 4 for the window and StatusNotifierItem (`ksni`)
for the tray. It supports Wayland/X11 desktops with a tray host, including
Hyprland with Waybar. The tray exists only while an Olympus TG-1 is connected
in USB Storage mode. Left-click opens the window; its menu offers Open and
Quit. Without a tray host, the window opens and explains the issue.

The dashboard shows:

- Camera connection, firmware, GPS interface, and reported battery level.
  These are communication checks, not an internal GPS diagnostic.
- Live storage capacity/free space and Linux mounts. Block-device capacity
  is available as a fallback when camera queries cannot run.
- Separate latest fitting and health-observation epochs, NOAA/IGS source and age,
  health-check age, and excluded satellites.
- The observed arc used to fit predictions, validity, weekly coverage, hash,
  fit/encoding RMS, and propagation time. RMS is not camera position accuracy.
- Last acknowledged commit, camera association, time, hash, validity,
  session-close status, and whether it matches the latest local predictions.
- Upload phase, progress, errors, and reasons an upload is blocked.

Camera connection starts ToughFix in the background; no login startup is needed.
A camera-triggered instance exits five seconds after disconnection if its window
is hidden. An open window stays available. A manually launched instance keeps
monitoring when its window is closed. Quit waits for an active camera operation
to finish. During queries, transfer, validation, commit, and close,
the tray is amber and says **Do not unplug**. Red indicates a camera error or
a required reconnection. Mounted storage must still be ejected afterward.

## Build and run

Build prerequisites: Rust 1.92+, a C toolchain, pkg-config, and GTK 4 development
packages. On Arch these are `rust`, `base-devel`, `pkgconf`, and `gtk4`.
Cargo.lock pins dependencies.

## Install or update

From this checkout, as your ordinary desktop user:

```sh
make install
```

This builds the locked release and uses the native installer to install `~/.local/bin/toughfix`, a ToughFix icon,
an application-menu launcher, and a user service activated by camera connection.
`XDG_DATA_HOME` and `XDG_CONFIG_HOME` are honored. The executable
is replaced atomically so installation does not interrupt an active upload.
An already running version remains active until you quit and reopen it.
Use the application menu to launch it, or `~/.local/bin/toughfix` if that
directory is not in your shell's PATH.

The first camera setup prompts for sudo to install the TG-1-specific udev rule
and `/etc/modules-load.d/toughfix.conf`, load the `sg` driver, and reload udev
rules. The rule asks systemd to start your user service when the camera enters
USB Storage mode, using [SYSTEMD_USER_WANTS](https://raw.githubusercontent.com/systemd/systemd/main/man/systemd.device.xml);
it never runs the GUI from udev or as root. Reconnect the camera once afterward. Subsequent connections grant the
active local desktop user access automatically; the app does not run as root.
Ordinary updates skip sudo when these files and the driver are already set up.
Do **not** run `sudo make install`.

In the window, **Settings → Start when camera connects** controls automatic
launching. It is enabled by default and changes take effect on the next camera
connection, without sudo. Installation updates preserve your choice. The switch
is unavailable in demo and monitor-only mode. The installer reloads the user
service definitions but never starts a camera job or quits a running app.

```sh
make install CAMERA_ACCESS=0 # Install user files without system setup
make install-camera-access   # Reapply camera setup only
```

Automatic launching requires systemd and an active `graphical-session.target`
with `DISPLAY` or `WAYLAND_DISPLAY` in the user manager environment (verified
on this Hyprland session). Other desktop sessions may need to import their
display environment. A camera already connected during installation must be
reconnected. The installer creates no XDG login autostart entry; legacy entries
created using earlier installer versions are left untouched.

The installed app and installer are entirely native Rust. No Python interpreter,
virtual environment, Nikon feed, or checkout is needed at runtime. Public inputs
and prediction files are cached in `$XDG_STATE_HOME/toughfix/engine` (normally
`~/.local/state/toughfix/engine`). The owner reports that the integrated Rust app
works. The earlier research upload has captured successful commit evidence;
a new instrumented native-camera capture is not recorded here.

`PREFIX`, `DATA_DIR`, and `CONFIG_DIR` customize user installation locations.
`DESTDIR` stages files without sudo, driver loading, udev reload, or app startup:

```sh
make install DESTDIR=/tmp/toughfix-stage PREFIX=/usr DATA_DIR=/usr/share
```

Run `make check` for Rust checks and installer tests, including a real GIO
launcher argument round trip, user-service parsing, safe updates, startup preference
preservation, staged camera setup, and data/health regressions. The tests never access a camera or invoke sudo.

## Run from the checkout

```sh
cargo build --locked
./target/debug/toughfix
```

Device-free preview:

```sh
./target/debug/toughfix --demo
```

Demo connect/disconnect and Simulate upload exercise tray visibility and
progress without device access, network requests, or real commit records.
Demo and real instances have distinct application IDs and state directories.

Live inspection with network refresh and uploads disabled:

```sh
./target/debug/toughfix --monitor-only
```

`--background` starts with the window hidden. Relaunching opens the existing
window. `--project PATH` selects an optional checkout for importing legacy
research commit history; `--state-dir PATH`
overrides `$XDG_STATE_HOME/toughfix` or `~/.local/state/toughfix`.
`--config-dir PATH` selects the preferences/service directory; installed launchers
pass their configured path.

The supplied `70-toughfix.rules` grants the active local desktop user access
only to the matching whole TG-1 disk and its SCSI generic endpoint. The installer
sets it up. Without access,
the dashboard explains the permission issue and blocks uploads.

## Automatic updates

Device discovery, telemetry, SCSI/PTP framing, uploads, GUI, tray, progress,
exclusive instance/device locks, and durable commit receipts are native Rust.
No Windows updater or firmware is executed.

When ToughFix starts normally, it refreshes official satellite notices and
observations, generates new orbit/clock predictions when the fitted inputs
change, independently decodes the quantized CEP, applies maneuver/outage
quarantine, and publishes an eligible candidate atomically. This repeats hourly
while the app is running; failures retry after five minutes. Downloads and
computation run separately from the camera monitor and GTK thread.

A connected camera automatically receives a changed eligible file when
**Update automatically when connected** is enabled (the default). Matching
confirmed commits are skipped. Disabling automatic uploads still permits data
refresh and manual uploads. **Refresh satellite data** runs the complete data
and prediction pipeline. A hidden camera-triggered app exits after disconnect,
so it does not perform scheduled work while it is absent; the next connection
refreshes the inputs. Closing an open dashboard keeps a manually launched app
running if you want ongoing refreshes.

Health uses the full current-year Coast Guard NANU archive and retained prior
references, the operational advisory, NOAA observed rapid/ultra-rapid GPS SP3
samples, USNO Earth orientation, and pinned NGA EGM96. Predicted SP3 positions
and clocks are never fitted. Rapid observations take priority on overlap;
observed Ultra-rapid data extend the three-day arc. Each satellite's clocks
pass separate residual gates or fall back to a recent Rapid fit; the dashboard
shows the clock-source counts. A complete three-day arc is required. Outages and
observed disruptions remove unsafe satellite weeks; recovery needs a clean new
fit. Network failures, malformed sources, stale health or observations, expired
predictions, CRC/slot errors, and fewer than four usable current-week satellites
block uploads. Existing assistance is never relabeled current.

The native `toughfix refresh` command exercises downloads, generation, guarding,
and publication **without camera access**:

```sh
~/.local/bin/toughfix refresh
~/.local/bin/toughfix refresh --offline # cached inputs; freshness still enforced
```

Before uploading, Rust freezes the exact bytes and repeats native health,
validity, hash, CRC, and decoded physical-range checks, including a second check
immediately before the transfer. It sends the verified `0x9128` through `0x912c`
sequence. It makes no normal filesystem/sector writes to the camera. Failed
uploads are not automatically retried; interrupted transactions need reconnect.
See the [README's numerical checks](../README.md#check-the-numerical-implementation-separately)
and [CEP format](../README.md#cep-file-format) for scientific validation and encoding details.

Vendor commands use the verified `/dev/sg*` character endpoint opened read/write;
Linux filters these commands on an unprivileged block-device descriptor. The
`sg` kernel module and the endpoint's udev access are required. The GTK process
does not need root or CAP_SYS_RAWIO. See the kernel's
[SCSI generic implementation](https://github.com/torvalds/linux/blob/master/drivers/scsi/sg.c).

`commits.json` is synced only after the camera acknowledges `0x912c`, before
session close. `attempt.json` preserves interruptions. Successful research
history is imported only after checking captured bytes and the commit response;
the private PTP identity capture binds a hashed camera identity. Raw serial
numbers are not displayed or copied into receipts. Another camera's history
never marks this camera current. There is no verified assistance readback;
history does not prove present contents or persistence across battery removal.

## Verification

```sh
cargo test --locked
cargo clippy --locked --all-targets -- -D warnings
```

Tests cover transfer reconstruction, commit ordering/timeout, rejection,
pending validation, premature commit, framing, camera-specific history, and
unplug warnings through close. `desktop/check_demo.py` also exercises the real
GTK/tray lifecycle, silent duplicate activation, and disconnect behavior without
a camera. The udev rule and generated user service pass their parsers; physical
USB-triggered launch should be checked after installation by reconnecting.
The owner reports the integrated app working; controlled acquisition and
long-term accuracy measurements remain open research work.
