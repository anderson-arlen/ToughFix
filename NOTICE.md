# License scope

Original ToughFix source code and documentation are MIT licensed; see
[LICENSE](LICENSE). That includes the original mathematical CEP translation
and the independent orbit predictor. This license does not grant rights to
third-party material or patents.

The public source repository does not include downloaded camera firmware,
updater binaries, vendor assistance files, private camera captures, or the
investigation archive. Those materials are not dependencies of the application.

Public data downloaded by the app retain their source terms. NOAA distributes
IGS orbit products from multiple analysis centers; government hosting does not
make NOAA the sole producer. USNO supplies Earth orientation, NGA supplies EGM96,
and NAVCEN supplies satellite notices. Source URLs, retrieval times, sizes, and
SHA-256 hashes are recorded in the app's data cache. The NGA archive is used only
for coefficient text; its included executable programs are not run.

The small public-data excerpts in `tests/fixtures` retain their original source
terms. Generated CEP fixtures, encoder expectations, and benchmark summaries
are original ToughFix work; no vendor assistance specimen is included. See
[fixture attribution](tests/fixtures/README.md).

`vendor/ode_solvers` contains ode_solvers 0.6.2 under Apache-2.0, with its
original LICENSE and source notices and a documented stage-time correction.
It is a dependency of the MIT-licensed ToughFix application, not relicensed
MIT code. `vendor/simba` contains Simba 0.9.1 under Apache-2.0, with unchanged
numerical source and a documented dependency substitution from the archived
`paste` macro crate to its maintained successor `pastey`. Its original license
and source notices are preserved. ERFA's bundled C code retains its BSD license; the erfars wrapper
is MIT/Apache-2.0. Cargo.lock identifies the other dependencies and versions.
