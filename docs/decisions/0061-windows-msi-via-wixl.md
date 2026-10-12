# Decision 0061: The Windows MSI is built with `wixl` (msitools), not `cargo-dist`/`cargo-packager`

**Status:** Accepted
**Amends:** 0055 (the Windows installer tool)
**Related:** decision 0055 (reuse existing code before writing new code, which named
`cargo-dist`/`cargo-packager` as the planned release pipeline)

This record exists so the Windows MSI's build tool is not re-argued the next time someone
notices it is `wixl` rather than the `cargo-dist`/`cargo-packager` pairing decision 0055
originally named for the release pipeline generally.

## Decision

`scripts/package-windows.sh` builds the Windows `.msi` with `wixl`, from the `msitools`
project (LGPL-2.1-or-later license), invoked as a subprocess build tool, not `cargo-dist`, not
`cargo-packager`, and not `cargo-wix`. A zip archive, always built regardless, is the fallback
artifact when `wixl` or its companion tool `msiinfo` are unavailable on the build machine.

## Context

Decision 0055 named `cargo-dist` (MIT or Apache-2.0 license) as the release pipeline, with
`cargo-packager` as a fallback for per-OS installers (`.dmg`, `.msi`, `.deb`) if `cargo-dist`
could not produce one directly. Neither tool, in practice, produces a Windows `.msi` from a
macOS build host:

- `cargo-dist` builds and publishes releases from continuous-integration infrastructure, but
  its own Windows `.msi` step runs the WiX toolset on a Windows CI runner. No CI runner,
  Windows or otherwise, was available to hand that step to.
- `cargo-packager` can drive an `.msi` build too, but it does so the same way `cargo-wix`
  does: by shelling out to Microsoft's own WiX toolset binaries (`candle.exe`/`light.exe`, or
  `wix.exe` in WiX v4 and later). Those are native Windows and .NET executables with no
  equivalent on macOS or Linux, so installing `cargo-packager` or `cargo-wix` on a macOS host
  changes nothing about the underlying problem.

So the actual choice was not "`wixl` versus `cargo-dist`/`cargo-packager`" as two equally
working alternatives; it was whether anything could produce a real, verifiable `.msi` from a
macOS host at all, given that the WiX-family Cargo wrappers all ultimately need
Windows-native binaries that neither macOS nor Linux ships.

## Alternatives considered

- **`cargo-dist`, waiting for CI availability.** The correct long-term path, and this decision
  does not reverse decision 0055's overall plan, but it produces nothing today.
- **`cargo-packager` or `cargo-wix`.** Both need Microsoft's native WiX binaries, none of which
  exist on macOS or Linux. Rejected for the current build host, not on their own merits;
  either becomes viable again the moment a Windows CI runner or a Windows development machine
  is available, and nothing in this decision blocks adopting one then.
- **Ship only a zip archive, with no installer.** Simpler, but loses Windows Add/Remove
  Programs registration, a Start Menu shortcut, and a system PATH entry: real usability
  regressions relative to what the macOS `.dmg` and Linux `.deb` packages both already
  provide.
- **`wixl` (msitools), chosen.** A real, independent implementation of the WiX XML schema (not
  an approximation, not a different format standing in for one) that runs natively on macOS
  and Linux and produces a genuine MSI file: a Composite Document File container, confirmed
  with the standard `file` utility reporting it as an "MSI Installer" and with `msiinfo`
  reading back the File, Component, Environment, Upgrade, and Shortcut tables this project's
  own installer definition writes. It is invoked here the same way this project's other
  packaging scripts already invoke OS-native build tools: a subprocess that compiles input into
  an output artifact and is never linked into or shipped with the kGraf binary, so the
  accepted-license list for shipped dependencies does not apply to it, the same reasoning that
  lets this project build with a GPL-licensed compiler without the product itself becoming
  GPL-licensed.

## Consequences

`scripts/package-windows.sh` requires the `msitools` package to produce a `.msi`; without it,
the script states why and falls back to the zip archive rather than silently downgrading.
`wixl`'s WiX-schema support is a subset of the full WiX v3/v4 feature surface (no bundle/burn
support, and some v4-only package attributes are not recognized; architecture is instead
selected with a command-line flag). The project's own installer definition file stays within
what `wixl` supports.

This decision is revisited, not necessarily replaced, once either a Windows CI runner becomes
available for `cargo-dist` to drive its own Microsoft-toolchain-backed `.msi` build, matching
decision 0055's original plan, or a real Windows development machine becomes available, at
which point `cargo-wix` becomes a genuine option since the Microsoft WiX binaries would finally
have something to run on. Neither event supersedes this decision on its own; `wixl` remains a
working, no-Windows-machine-needed path either way, and having a CI-produced `.msi` in addition
does not make the local one wrong.

## Addendum: no other permissive, suitable tool was found

A search of the alternatives, with licenses checked at the source, found none that is both
permissively licensed and suitable:

- `cargo-packager`, `cargo-dist`, `cargo-wix`, and the WiX toolset itself (versions 4 through
  6) all need native Windows or .NET binaries, and current WiX versions are under the
  Microsoft Reciprocal License or a commercial license, outside the accepted-license list for
  shipped dependencies.
- An MIT-licensed crate that writes raw MSI tables directly exists, but using it means
  rebuilding logic `wixl` already provides correctly, including table-flag handling this
  project already had to work through once.
- Inno Setup needs Windows or a Windows compatibility layer, and its license is not on the
  accepted list.
- NSIS, with a permissively licensed environment-variable plugin, runs on macOS and meets the
  install requirements, but it produces a plain `.exe` installer with no MSI upgrade model and
  no table-level verification. It is the fallback if `wixl` ever becomes unusable.

`wixl` stays, as a build tool only.
