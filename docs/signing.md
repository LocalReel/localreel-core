# Windows code signing

`scripts/sign.ps1` wraps `signtool.exe` (Windows SDK) for the exe and MSI.

## Real certificate

```powershell
cd localreel-core
.\scripts\sign.ps1 `
  -Path .\dist\localreel-0.1.0-x64\localreel.exe, `
        .\dist\localreel-0.1.0-x64.msi `
  -PfxPath .\secrets\localreel.pfx `
  -PfxPassword '<password>'
```

The script signs every path with SHA-256 and a timestamp
(`http://timestamp.digicert.com`), then prints the embedded signer and
`Get-AuthenticodeSignature` status. A trusted certificate should show `Valid`.

Release order:

1. Build the zip stage and MSI (`package.ps1`, `build-msi.ps1`).
2. Sign the **exe in the stage** and the MSI.
3. Re-create the zip from the stage (the original zip contains the unsigned
   exe), then refresh `.sha256`.
4. Publish the signed MSI/exe and hashes.

For a fully automated pipeline, add a signing step before
`Compress-Archive`; do not sign the zip itself (Windows verifies the exe/MSI,
not the archive).

## Local self-signed test

```powershell
.\scripts\sign.ps1 `
  -Path .\tmp\localreel.exe, .\tmp\localreel.msi `
  -SelfSignedTest
```

This creates a temporary CurrentUser code-signing certificate, exports a
temporary PFX, signs the copies, prints the signer, then deletes the PFX and
certificate. `Get-AuthenticodeSignature` will report `UnknownError`/`NotTrusted`
because the test cert is not in the trust store; that is expected. **Do not
distribute self-signed artifacts as if they were trusted.**

If the test certificate is manually trusted (not done by this script), the
status becomes `Valid`; remove it from `Cert:\CurrentUser\Root` afterwards.

## Requirements and caveats

- `signtool.exe` from the Windows SDK (`x64`).
- A CA-issued code-signing certificate (OV/EV) or an internal enterprise CA.
- Timestamping requires network access; add `-NoTimestamp` only for local
  experiments.
- The current MSI/exe in `dist/` are **unsigned**; code signing remains a
  v1.0 release blocker.
