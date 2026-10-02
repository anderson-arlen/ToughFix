# ToughFix desktop

The Rust application uses GTK 4 with Libadwaita for the window and StatusNotifierItem (`ksni`)
for the tray. It supports Wayland/X11 desktops with a tray host, including
Hyprland with Waybar. The tray exists only while a supported Olympus camera is connected
in USB Storage mode. Left-click opens the window; its menu offers Open and
Quit. Without a tray host, the window opens and explains the issue.

Activity and progress stay pinned above the scrolling content, including in a
small window or while viewing Advanced details. The main view shows an embedded
camera illustration and health snapshot, a colored battery-shaped gauge,
an SD card graphic showing capacity, a storage-use bar and free space,
GPS assistance summary with a slowly rotating globe and orbiting satellite
dots, and last confirmed update. The illustration pauses when hidden and
respects the desktop animation preference. Idle connection status
appears only in the activity panel. **Camera**, **Advanced**, and **Settings**
are tabs in one window; **Settings** contains the
startup, automatic-update and upload-interval preferences. **Update GPS now** in
the GPS assistance section refreshes inputs and updates the camera; its adjacent
refresh icon updates only local observations and predictions. **Advanced**
retains the explicit rewrite control. The refresh icon beside the snapshot age reads a new
battery, health and SD-card snapshot without uploading GPS data. It briefly
unmounts and remounts the card; a busy card is left alone. Snapshot age appears
beside health and beneath the battery gauge, and advances while the window is open.
Navigation uses Libadwaita's native icon-and-label view switcher in the header,
with a bottom navigation bar below 520 logical pixels. Native controls keep
their theme styling; custom CSS is limited to content cards and labels.

The main window starts at 560 × 660. On Hyprland, ToughFix requests floating
placement for its own window each time it opens, without editing compositor
configuration. Reopening retains the size used before hiding. Resizes and
placement changes while the window is open are left to the user. Other
desktops retain their normal placement policy. The battery bolt means USB is
connected; active charging is not exposed by the camera. Unknown battery and
free-space readings are shown as unavailable, rather than zero or empty.

The activity panel identifies satellite health checks, observation downloads,
calculation, validation, and every camera upload stage. Downloads and checks
use animated progress; calculations count completed satellites, and transfers
report actual bytes. Indeterminate stages never show a made-up percentage.
Camera errors and mount failures appear at the top too.

**Advanced** shows:

- Camera connection, firmware, GPS interface, and reported battery level.
  Camera readings are timestamped snapshots from the initial unmounted check
  or an upload, or an explicitly requested camera refresh; status is not polled
  through vendor sessions.
- Live mounted-storage capacity/free space from Linux filesystem statistics,
  and Linux mounts. Block-device capacity is available as a fallback.
- Separate latest fitting and health-observation epochs, NOAA/IGS source and age,
  health-check age, and excluded satellites.
- The observed arc used to fit predictions, validity, weekly coverage, hash,
  fit/encoding RMS, and propagation time. RMS is not camera position accuracy.
- Last acknowledged commit, camera association, time, hash, validity,
  session-close status, and whether it matches the latest local predictions.
- Upload phase, progress, errors, and reasons an upload is blocked.

Camera connection starts ToughFix in the background; no login startup is needed.
A hidden instance exits five seconds after disconnection. An open window stays
available. Closing the window hides it while a camera is connected; closing
without a camera quits the app, including manually launched instances. There
is no window Quit button; Quit remains in the tray menu and waits for an active camera operation
to finish. During queries, transfer, validation, commit, and close,
the tray is amber and says **Do not unplug**. Red indicates a camera error or
a required reconnection. Mounted storage must still be ejected afterward.

## Build and run

Build prerequisites: Rust 1.92+, a C toolchain, pkg-config, and development
packages for GTK 4 and Libadwaita 1.4+. On Arch these are `rust`, `base-devel`,
`pkgconf`, `gtk4`, and `libadwaita`.
UDisks2 (`udisks2` on Arch) is required for mounting camera storage after the
connection-time update.
Cargo.lock pins dependencies.

## Install or update

On Arch, a GitHub release provides an x86_64 `.pkg.tar.zst` and `SHA256SUMS`.
Install it with `sudo pacman -U ./toughfix-*.pkg.tar.zst`. The package owns
`/usr/bin/toughfix`, the launcher, icon, camera unit, udev rule, driver setting,
and MIT license. Package hooks reload udev and logged-in users' service definitions
without restarting an active camera operation. Preferences still use
`$XDG_CONFIG_HOME` or `~/.config`, and runtime data stays in the user's state directory.

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

Camera setup prompts for sudo to install model-specific udev rules
and `/etc/modules-load.d/toughfix.conf`, load the `sg` driver, and reload udev
rules. The rule asks systemd to start your user service when the camera enters
USB Storage mode, using [SYSTEMD_USER_WANTS](https://raw.githubusercontent.com/systemd/systemd/main/man/systemd.device.xml);
it never runs the GUI from udev or as root. Reconnect the camera once afterward. Subsequent connections grant the
active local desktop user access automatically; the app does not run as root.
Ordinary updates skip sudo when these files and the driver are already set up.
An update that changes the camera rule asks for sudo again to replace it.
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
only to matching TG-1 and Stylus Tough-8010 disks and SCSI generic endpoints. The installer
sets it up. Without access,
the dashboard explains the permission issue and blocks uploads.

## Automatic updates

Device discovery, telemetry, SCSI/PTP framing, uploads, GUI, tray, progress,
exclusive instance/device locks, and durable commit receipts are native Rust.
No Windows updater or firmware is executed.

When ToughFix starts normally, it refreshes official satellite notices and
observations, generates new orbit/clock predictions when the fitted inputs
change, independently decodes the quantized CEP, applies maneuver/outage
quarantine, and publishes an eligible candidate atomically. This repeats every
30 minutes while the app is running, earlier if needed to keep health checks
fresh. Failures show their source, cause, and next automatic retry in the status
banner. Retries start after 30 seconds and back off to five minutes; connecting
a GPS camera with unavailable data requests an immediate attempt. Downloads and
computation run separately from the camera monitor and GTK thread.

A connected camera automatically receives a changed eligible file, normally no more
than once every 48 hours since its last confirmed commit, when
**Update automatically when connected** is enabled (the default). The
model-specific udev rules set [UDISKS_AUTO](https://storaged.org/udisks/docs/udisks.8.html)
to hold desktop automount while this initial update runs. Once it finishes,
ToughFix mounts the card through UDisks2, ready for browsing. If storage has
already mounted, ToughFix requests one normal unmount during this initial
check, including a health and battery reading. A busy card cancels the check;
partitions already unmounted are restored. No forced unmount occurs. Mounts
that appear after this preparation cancel further automatic camera access.
After the initial check, browsing storage is never interrupted automatically.
Camera identity, battery and SD card checks run before GPS-specific commands;
a failed storage check prevents the GPS update.
The app permits one automatic upload per USB attachment; later prediction
refreshes leave the camera alone until a manual upload or a new connection.
A brief SCSI disk reattachment preserves the USB attachment identity and does
not restart status polling. A matching acknowledged commit skips the transfer;
a health/battery check still runs once before storage mounts. A real USB serial
identifies history through an explicit prior link or an identical saved PTP
serial hash. Old receipts are linked during the next identified session without
changing the recorded commit time. Health snapshots are saved by USB serial
and shown with their original timestamp after a restart; they are never
reused for another camera or presented as live polling. This is acknowledged upload history,
not readback of the file stored on the camera.

An unsuccessful initial refresh still permits one health/battery check before
releasing storage; a 90-second wait limit also
releases it if sources or calculation take too long. An active upload finishes
and closes its session before mounting. A late forecast waits until the next
connection. Disabling automatic updates permits one initial status snapshot,
then mounts the card. [ExecStopPost](https://www.freedesktop.org/software/systemd/man/latest/systemd.service.html#ExecStopPost=)
service cleanup mounts storage when startup is disabled or the app fails; it
avoids racing an existing ToughFix instance. Mounting uses the desktop user's
UDisks authorization, without sudo or an authentication prompt. If automatic
mounting fails, the dashboard reports it and the card can be opened manually
in a file manager.

Settings provides a 1–168-hour minimum interval between automatic camera uploads
(default 48 hours). This is measured from confirmed commits to the matching
camera. Less than 48 hours of remaining validity or new satellite exclusions
bypass the interval. Old receipts recover their exclusions from the exact
hash-verified local archive when available; uncertain health history never
postpones a necessary update.

**Update GPS now** fetches current inputs before uploading, ignores the interval,
and skips identical data. It performs one normal unmount and restores the card
on success or failure; it never forces a busy filesystem. A queued update is
cancelled if the camera changes or disconnects. Advanced retains **Upload again**
for an explicit rewrite.

Disabling automatic uploads still permits data
refresh and manual uploads. **Refresh satellite data** runs the complete data
and prediction pipeline; it does not repeat camera queries. A hidden
camera-triggered app exits after disconnect,
so it does not perform scheduled work while it is absent; the next connection
refreshes the inputs. Closing an open dashboard keeps a manually launched app
running if you want ongoing refreshes.

Health uses the full current-year Coast Guard NANU archive and retained prior
references, the operational advisory, NOAA observed rapid/ultra-rapid GPS SP3
samples, IERS Earth orientation (with USNO as backup), and pinned NGA EGM96. Predicted SP3 positions
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
