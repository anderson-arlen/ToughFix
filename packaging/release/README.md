# Arch release packaging

`.github/workflows/release.yml` follows the PacSmith and CephFS mount-helper
release setup: official Arch container, unprivileged build, staged installation,
`makepkg`, metadata checks, and package/checksum assets on GitHub Releases.
It runs for pushed `v*` tags and published releases. Tag validation accepts
`vMAJOR.MINOR.PATCH` with optional `-alpha.N`, `-beta.N`, or `-rc.N`.

The workflow sets `TOUGHFIX_BUILD_VERSION` for the executable and
`TOUGHFIX_RELEASE_VERSION` for the pacman package. The latter removes the
prerelease dash and dot so pacman's version ordering matches the other projects.
It does not modify `Cargo.toml` or the locked dependency graph.

To stage a package locally after building a release:

```sh
mkdir -p release-package
cp packaging/release/{PKGBUILD,toughfix.install} release-package/
./target/release/toughfix install --system-package \
  --destdir "$PWD/release-package/stage"
(cd release-package && TOUGHFIX_RELEASE_VERSION=0.1.0 makepkg --clean --force)
```

Run `makepkg` as an ordinary user. Staging requires `--destdir` and performs no
system setup or camera access. Shared files live under `/usr`; the service uses
systemd's `%E` user-config specifier for the startup preference. The launcher
and camera service resolve user configuration normally instead of directing
settings writes into `/etc`.

The package hooks reload rules and existing user managers and load the `sg`
driver, but never restart ToughFix or open a camera. Reconnect after installation
and quit/reopen a running app after its operation finishes when upgrading.
