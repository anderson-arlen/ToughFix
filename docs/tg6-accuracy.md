# TG-6: recent assistance-data accuracy

Evaluated 2026-10-14 (local date). **ToughFix has lower measured errors than the
current OM TG-6 file in this short, retrospective comparison.** This does not
establish a winner over the complete two-week prediction period or measure
the camera's position accuracy.

Both files were checked against the **same observed IGS measurements**, rather
than treating OM's predictions as truth. The evaluation covers **2026-10-14
00:00–17:45 GPS**. Only the observed half of the Ultra-rapid product is used;
predicted samples are excluded by both flags and the product's time interval.
[IGS documents the observed/predicted split and product accuracy](https://igs.org/products/).

## Results including TG-6 encoding

The independently generated TG-6 candidate contains 20 satellites, of which
18 also occur in OM's file. Its positions and clocks were decoded from the
actual generated binary before scoring. Satellites whose selected native
clock model is quadratic cannot be represented by the recovered linear-clock
format and were excluded **before** evaluation, using the existing export
rule. The same 18 satellites and timestamps are used on both sides.

| Error against observations | ToughFix TG-6 candidate | OM TG-6 file |
| --- | ---: | ---: |
| Satellite position, 3D RMS | **0.524 m** | 0.583 m |
| Position, 95th percentile | **0.717 m** | 0.862 m |
| Position, maximum | **2.204 m** | 2.242 m |
| Satellite clock RMS | **2.172 ns** | 3.322 ns |
| Clock, 95th absolute percentile | **4.472 ns** | 8.812 ns |
| Clock, maximum absolute | **10.817 ns** | 16.470 ns |

There are **162 paired orbit samples** and **1296 paired clock samples**.
ToughFix's RMS errors are approximately **10% lower for position** and **35%
lower for clocks** in this representable subset. Clock errors are relative to
the IGS timescale; no common offsets or per-satellite biases were removed.

## Full native calculation

Before the linear-clock format restrictions, all **28 satellites** in OM's
file can also be compared with the native ToughFix calculation. This includes
the native predictor's selected quadratic clocks, so it is a calculation
benchmark, distinct from the encoded TG-6 subset above.

| Error against observations | ToughFix native | OM TG-6 file |
| --- | ---: | ---: |
| Satellite position, 3D RMS | **0.471 m** | 0.538 m |
| Position, 95th percentile | **0.678 m** | 0.712 m |
| Satellite clock RMS | **1.774 ns** | 4.588 ns |
| Clock, 95th absolute percentile | **3.375 ns** | 9.747 ns |

This comparison uses **252 paired orbit samples** and **2016 paired clock
samples**. ToughFix has lower orbit RMS on 22 of 28 satellites and lower clock
RMS on 23 of 28. The aggregate advantage occurs in each six-hour block, rather
than depending on one favorable time interval. Individual errors and maxima
are retained in the research results.

![Native ToughFix and OM errors against observed IGS samples](figures/tg6-observed-comparison.png)

## What the result establishes

The native forecast was rebuilt from three days of satellite observations
strictly before **2026-10-14 00:00 GPS**. Its last fitted orbit and clock samples
are at **2026-10-13 23:45 GPS**. No evaluation-period satellite samples or OM
coefficients were used to fit it. The current IERS Earth orientation was used
for both the native fit and reference-frame conversion. An independent rescore
using the native terrestrial positions agrees with the inertial-coordinate
scorer within **9 nanometres**; that is an implementation consistency check,
not a claim of nanometre physical accuracy.

The conclusion is limited in several ways:

* This is a **retrospective fit**, not a reconstruction of what was available
  in real time at midnight. Some fitted IGS products arrived after the cutoff,
  and the Earth-orientation file was also obtained later.
* OM's file has HTTP Last-Modified **2026-10-14 19:00:37 UTC**, after the entire
  evaluation window. Its actual training cutoff is unknown. Its clock reference
  epoch is October 12, but a reference epoch does not identify the last fitted
  observation. Equal information availability or prediction age is not proven.
* Orbit scoring uses exact two-hour state knots through 16:00 GPS. It does not
  assume an unverified receiver interpolation algorithm between knots. Clock
  scoring uses 15-minute observations through 17:45 GPS.
* The reference is observed IGS Ultra-rapid, rather than a later Final product.
  More days of held-out observations are needed to assess forecast aging.
* Satellite assistance errors do not directly measure camera location error,
  acquisition time, receiver acceptance, or the unresolved ancillary tables.

The encoded candidate remains an offline research artifact. No upload or
change to operational camera support was made for this comparison. The frozen
files can be rescored against later observations without refitting, to measure
how these particular forecasts age.

Subsequent work ported the encoder into the native app and completed a
successful camera upload. See the separate
[hardware validation record](tg6-format.md#native-integration-and-hardware-validation);
that upload does not extend the accuracy evidence reported here.

## Reproducibility

The scorer, paired CSV, full JSON, and frozen native diagnostics are preserved
locally under `research/tg6/accuracy/` (the existing ignored research archive).
The scorer is `research/tg6/accuracy_compare.py`; the generator and decoder are
`research/tg6/generate.py` and `research/tg6/sony_cep.py`. The research suite
passes 14 tests, including checks that unflagged predicted Ultra-rapid halves
are rejected and position/clock prediction flags are handled independently.
Public summary statistics and input hashes are in
[tg6-accuracy-data.json](figures/tg6-accuracy-data.json).

Key inputs are pinned by SHA-256:

| Input | SHA-256 |
| --- | --- |
| Official `cep_pak.bin` | `09a0f53b3d20bcd8d982e403be717834c7942cec70c1f73c70ed5a48b5965993` |
| Independent encoded candidate | `cc0c404a2f2632db8884ec46abee799062db080897aa6888db73acd52bb107bd` |
| Observed `igu24403_18.sp3.gz` | `cd598378bb9769aab7b2418de87bb2439cae369d6511ca97466981552433aa37` |
| IERS `finals2000A.all` | `0baf00f67a39ab362a3e4f07d2a1f084842178523f56bcf5a861e7ac084d411a` |

The reference was obtained from
[NOAA's public archive](https://noaa-cors-pds.s3.amazonaws.com/rinex/2026/287/igu24403_18.sp3.gz);
the official specimen came from the endpoint recovered from OM Workspace,
as described in the [format investigation](tg6-format.md).
