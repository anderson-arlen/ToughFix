# TG-6 assistance format investigation

Investigated 2026-10-14. The TG-6 uses a different assistance format from the
TG-1. The core selector-3 container, satellite states, clocks, and checksum are
decoded, and the native app produces independent two-week assistance files.
A generated file has passed the connected camera's validation and commit.
The owner then reported an outdoor GPS fix in roughly five seconds. Receiver
interpolation and ancillary consumer code have not been recovered. See the
hardware record below for the precise scope of this evidence.

## Hardware evidence

The connected Olympus TG-6, firmware 1.00, identifies in USB Storage mode as
`07b4:012e`, SCSI vendor `OLYMPUS`, model `TG-6`. An information-only session
successfully used the existing C0/C2/C3/C4 SCSI tunnel. DeviceInfo advertises
the assistance operations `0x9126`–`0x912c`.

* `0x9126`: selector **3**.
* `0x9127`: transfer capacity **262144 bytes**.

The session closed successfully and the originally mounted card was remounted.
That initial information session performed no assistance, firmware, or sector
writes. The later native assistance upload is documented below. A selector number
does not establish the exact receiver chip part number.

## Official download path

The official Workspace installer was obtained through the
[OM download page](https://download.omsystem.com/pages/owdownload/en/) using
the connected camera's serial number. Its PE resources identify **2.5.0.11**.
Static extraction passes through three CAB layers to
`application/plugins/FNP_AGPS.ofnp`; no vendor executable was run or loaded.

The plugin's download branch, at virtual addresses `0x180005c82`–`0x180005cd3`,
selects these endpoints:

| Selector | Official endpoint |
| --- | --- |
| 3 | `https://agps.om-digitalsolutions.net/cep_pak.bin` |
| 4 | `https://agps.om-digitalsolutions.net/lle_gps.lle` |

The request includes the `x-api-key` client header embedded in the plugin.
An unauthenticated request returned 403; reproducing the embedded header
returned 200. The HTTP helper's User-Agent is `OV2 HttpCtrl`. The plugin sends
the downloaded main buffer unchanged; it does not calculate orbit predictions.
Its selector-1 CRC and selector-2 MD5 branches are not selector-3 file checks.

The selector-3 specimen was last modified **2026-10-14 19:00:37 UTC**:

| Property | Value |
| --- | --- |
| Downloaded size | 262144 bytes |
| Logical size, including checksum | 228304 bytes |
| Ancillary region end / satellite region start | 1836 (`0x72c`) |
| Zero padding | 33840 bytes |
| Satellite records | 28, each 8088 bytes |
| Samples per satellite | 336 at two-hour spacing, nominally 28 days |
| First orbit sample, GPS calendar | 2026-10-14 00:00:00 |
| SHA-256 | `09a0f53b3d20bcd8d982e403be717834c7942cec70c1f73c70ed5a48b5965993` |

The separately preserved Quectel example `cep_pak.bin` is a second instance of
this same format: 32 satellites, 168 states each, 14 days, 130856 logical bytes,
padded to 131072 bytes. Its timestamps are in November 2025 even though the
example was downloaded in September 2026. A current download is not sufficient
evidence of current prediction dates.

## Container

All integers are little-endian. Satellite calendar times use **continuous GPS
calendar**, without subtracting leap seconds. A time tick is 900 seconds since
MJD epoch 1858-11-17; GPS seconds are `(ticks - 44244*96)*900`.

| Offset | Type | Meaning |
| --- | --- | --- |
| 0 | `u16` | Container tag `0x122a` |
| 2 | `u16` | Byte offset of the first satellite record |
| 4 | `u32` | Logical file length, including the final four-byte checksum |
| 8 | variable | Five ancillary tables in the order below |
| header offset 2 | variable | Satellite records |
| logical length − 4 | `u32` | Sum of every preceding byte, modulo 2^32 |
| logical length | bytes | Zero padding to the transfer buffer size |

The checksum is a byte sum, not a CRC, MD5, or signature. It is **29030783** for
the official specimen. Both preserved specimens round-trip byte for byte through
the reconstructed reader/writer, including their checksums and padding.

Each ancillary table has an eight-byte header: `u32 epoch`, `i16 step`, and
`u16 scalar_count`, followed by that many signed 32-bit integers. Counts describe
individual scalar values, rather than vector rows.

| Table | Components | Observed step | Official scalars | Interpretation |
| --- | ---: | ---: | ---: | --- |
| Earth heliocentric position | 3 | 4 | 36 | J2000/ICRS-like XYZ, kilometres, four-day samples |
| Moon geocentric position | 3 | −1 | 186 | XYZ, metres, half-day samples |
| TAI−UT1 | 1 | 1 | 33 | Microseconds, daily samples |
| Polar motion | 2 | 1 | 66 | X/Y, 10^-5 arcseconds, daily samples |
| Nutation | 2 | −1 | 126 | Longitude/obliquity, approximately 10^-4 arcseconds, half-day samples |

These interpretations come from independent astronomy and Earth-orientation
comparisons. They have not all been established by tracing a receiver-side
consumer. In particular, the Sun and Moon values agree with ERFA at dates **one
day before** the nominal table-header dates. The independent exporter records
this as a specimen-derived convention; a validated interpretation is still
needed before hardware use. The nutation model also differs from modern ERFA
by roughly hundredths of an arcsecond. No exact model identity is claimed.

## Satellite records

| Record offset | Type | Meaning |
| --- | --- | --- |
| 0 | `u16` | `0x802a + ((PRN − 1) << 8)` for GPS PRNs 1–32 |
| 2 | `u16` | Record length: `24 + 24*state_count` |
| 4 | `u32` | Clock reference epoch, 900-second MJD ticks |
| 8 | `i32` | Clock bias, 10^-12 seconds |
| 12 | `i32` | Linear clock rate, 10^-20 seconds/second |
| 16 | `u32` | First state epoch, 900-second MJD ticks |
| 20 | `u16` | Sample spacing in seconds, observed as 7200 |
| 22 | `u16` | State count: 168 or 336 in the two specimens |
| 24 | repeated `6*i32` | Inertial XYZ in 0.1 m, then XYZ velocity in 0.001 m/s |

The state vectors agree with GCRS coordinates obtained from observed IGS SP3
using ERFA and IERS Earth orientation. Missing PRNs have no record; records are
not a fixed array of all 32 satellites.

For elapsed GPS seconds `dt` from the record's clock reference:

```text
clock_seconds = bias_integer * 1e-12 + rate_integer * 1e-20 * dt
```

For PRN 1, the first official state decodes to:

```text
position (m):   22706392.7, -13676074.9, -119155.4
velocity (m/s): 1168.343,    1914.703,    3168.240
clock reference: 2026-10-12 00:00:00 GPS
clock bias:       0.000151778562 seconds
clock rate:      -8.58113450e-12 seconds/second
```

The official states span 2026-10-14 00:00 through 2026-11-10 22:00 GPS; the
nominal end after 336 two-hour slots is 2026-11-11 00:00. The final sample is not
an extra endpoint. Receiver interpolation and its precise usable end boundary
remain to be recovered.

## Independent checks and generation

The reference reader checks bounds, declared lengths, table widths, satellite
markers, duplicate PRNs, record sizes, state counts, sample spacing, padding,
and the whole-file checksum. Mutation tests include malformed data with a
recomputed valid checksum, so checksum agreement cannot conceal structural errors.

Using current IERS data, 252 official state samples agree with independent
**observed** IGS Ultra-rapid positions to **0.538 m RMS**, maximum **2.242 m**.
An earlier conversion using cached Earth orientation gave 2.682 m RMS; the
difference shows why the frame data matter. The linear clocks agree with 4704
Rapid/observed Ultra-rapid measurements to **3.221 ns RMS**, maximum **16.471 ns**.
Predicted SP3 halves are excluded by both flags and Ultra-rapid filename epoch.

A separate [paired accuracy evaluation](tg6-accuracy.md) compares this file
with an independent native forecast and its actual TG-6 encoding against the
same held-out observed samples. ToughFix has lower errors over that recent
18-hour window; the evaluation documents the retrospective inputs and unknown
vendor training cutoff, which prevent a long-range forecasting claim.

The native predictor now exports `trajectory_2h_gcrs` alongside its existing
diagnostics: 168 position/velocity states across its two-week forecast. This
uses the existing numerical trajectory directly and does not alter TG-1 uploads.
The research exporter uses these independently fitted states, ERFA astronomy,
and the same public IERS input as the native fit. Vendor files are not inputs.

The 2026-10-14 18:00 GPS experiment produced an offline 256-KiB candidate for
18 satellites. Satellites whose selected clock fits are quadratic or outside
the linear format's integer range are excluded. The encoder reanchors eligible
linear clocks from the native cutoff-plus-three-hours reference to the first
state epoch. Across 3024 states, maximum encoding error was **0.0805 m** in
position, **0.000843 m/s** in velocity, and **0.492 ps** in clock evaluation.

That original offline prototype was not uploaded. It was subsequently ported
to the native app, connected to live health authorization, and tested on the
camera as described below.

## Native integration and hardware validation

The native codec is in [`src/predictor/tg6.rs`](../src/predictor/tg6.rs). It
decodes and rewrites both preserved vendor specimens byte for byte. Given the
same independent native diagnostics and Earth-orientation input, its generated
file is **byte-identical** to the independent Python encoder, including every
ancillary table. Public tests use an independently generated fixture, not a
vendor assistance excerpt.

Each prediction refresh generates separate TG-1 and TG-6 files from the same
native fit. The published TG-6 file is bound to the hash of the current native
health-guarded archive. Only satellites usable in **both** guarded weeks are
retained, because each TG-6 record spans the whole fortnight. Stale health,
NANU outages, observed discontinuities, fewer than four eligible satellites,
altered files, and cross-model file sizes block an upload. Selected quadratic
clocks remain excluded instead of silently losing their quadratic term.

The backend selects the prediction pointer and format by camera model. It
uses the normal storage preparation/restoration, exclusive transport lock,
identity/selector checks, validation polling, commit acknowledgement, and
per-camera receipts. The udev rules include the verified `07b4:012e` TG-6
Storage endpoint. The installed runtime needs no Python, vendor updater, or
vendor assistance server.

On **2026-10-15 at 01:08:19 UTC** (October 14 locally), the connected TG-6,
firmware 1.00, accepted and committed the first native health-cleared file:

| Property | Result |
| --- | --- |
| Size | 262144 bytes |
| Satellite records | 17 |
| Start / nominal exclusive end, GPS | 2026-10-14 18:00 / 2026-10-28 18:00 |
| SHA-256 | `323ef0a2c31eee5da9865d79e2349bba6ba202e8d606c00e05fb7c0d50aa5759` |
| Camera validation | State 2, accepted |
| Commit | Acknowledged successfully |
| Session close | Successful |
| Card mount | Restored successfully |
| Outdoor GPS check | Owner reported a fix in roughly five seconds after unplugging |

The file contained only independently calculated predictions and astronomy,
plus public IERS inputs. PRN 31 was removed by the live health guard; other
omissions were due to the linear-clock format. No firmware or normal-sector
write occurred. The full private receipt and immutable archive are preserved
under `research/tg6/runtime/`.
The [public hardware validation summary](tg6-hardware-validation.json) omits
the camera identity keys and private captures.

**Limits:** a successful file check and commit establish USB upload acceptance,
not the receiver's interpolation algorithm, the exact ancillary consumer time
conventions, acquisition improvement, or camera position accuracy. The outdoor
check confirms that positioning worked after this generated file was uploaded;
it was not a controlled cold-start test with an unaided baseline. The Sun/Moon
look-back convention is supported by
both specimens, but remains an inference until consumer code or a controlled
receiver experiment verifies it. The native exporter uses the documented
ERFA/IERS interpretation; it does not copy vendor tables.

For headless checks, the same guarded native path is available as:

```sh
toughfix camera info
toughfix refresh
toughfix camera update
```

`camera update` accepts no arbitrary file and writes only the current
health-validated prediction for the attached model. The offline codec is
available through `toughfix tg6 inspect FILE` and `toughfix tg6 encode --help`
(see `toughfix tg6` for its required paths).

Local reproducibility scripts and pinned specimens are in the ignored
`research/tg6/` investigation archive. Vendor installers, firmware, payloads,
camera serial captures, and large observation files remain outside the public
source tree. The public format description above does not require those files
to understand the container or satellite units.
