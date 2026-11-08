# ADR-015: The Windows per-user installer is ours, not MSI

- Status: Accepted
- Date: 2026-11-08

## Context

LocalReel is a household application: one person installs it, several people
use it, and the data of each account lives in that account's profile. The
installer should therefore install per-user and never ask for administrator
rights. The MSI declared `Scope="perUser"`, and the documentation promised "no
administrator rights".

It did not hold. Windows Installer promotes a package marked per-user to a
machine-wide registration when an administrator runs it. The same package then
registers in HKLM while its files sit in one user's profile: uninstallable only
by that user, unreachable for everyone else. Verified on a normal Windows 11
machine: `LUASetting=0`, `ProductDeploymentFlags=2`, an HKLM uninstall entry,
and no HKCU one.

The ways out were tried and measured:

- explicit `ALLUSERS=2` + `MSIINSTALLPERUSER=1`: the engine deletes ALLUSERS and
  ignores MSIINSTALLPERUSER ("not valid for UAC compliant package");
- the same properties on the command line: deleted as well;
- an empty `ALLUSERS`, the documented per-user switch: WiX refuses an empty
  property value, and the MSI API cannot store an empty string;
- a WiX Burn bundle with `MsiPackage ForcePerMachine="no"`: Burn has no
  force-per-user switch, and the MSI still registered in HKLM.

MSI, with or without a bootstrapper, cannot promise what the documentation
promised.

## Decision

Ship a small installer of our own for the per-user case:
`localreel-<version>-x64-setup.exe`.

- It embeds the staged payload and writes only inside the current user's
  profile: `%LOCALAPPDATA%\Programs\LocalReel` for the program,
  `%APPDATA%\...\Start Menu\Programs\LocalReel.lnk` and the desktop shortcut, and
  one HKCU uninstall entry under a fixed key.
- It does not touch the library in `%LOCALAPPDATA%\LocalReel`; uninstall removes
  the program and keeps the data unless `/removedata` is passed.
- Installing a newer setup over an older one replaces the program in place.
- The UI text follows the Windows display language; Chinese and English today,
  selected by a dictionary that fails to compile when a language is incomplete.

The MSI stays, documented as the all-users installation for machines that want
one copy for several accounts.

## Consequences

- The recommended download is a program we maintain, so its behaviour is
  testable and predictable; there is no dependency on Windows Installer's scope
  rules.
- Signing is now our concern for one more artifact (currently unsigned, like the
  MSI).
- Localisation lives in the installer crate. Adding a language is one dictionary
  and one detection arm, not a new build per language.
