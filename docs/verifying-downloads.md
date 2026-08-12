# Verifying downloads

Every released `bemtvi` binary ships with a SHA-256 checksum and a signed
[build provenance attestation](https://docs.github.com/actions/security-guides/using-artifact-attestations)
proving it was built by this repository's release workflow.

## Checksums

Each release (stable and `edge`) includes a `SHA256SUMS` file. After downloading
an archive into the same directory:

```sh
sha256sum --ignore-missing -c SHA256SUMS
```

## Provenance attestation

Requires the [GitHub CLI](https://cli.github.com). Verify an archive against the
attestation GitHub stores for it:

```sh
gh attestation verify bemtvi-0.2.0-x86_64-linux.tar.gz --repo davidrios/bemtvi
```

A successful run confirms the artifact was produced by the `bemtvi` release
workflow at a specific commit, and was not tampered with afterwards.

## macOS signature & notarization

The macOS binaries are signed with an Apple **Developer ID Application** certificate, built
with the hardened runtime, and **notarized** by Apple, so they run on any Mac without a
Gatekeeper override. Confirm locally:

```sh
# Signature, authority chain, hardened runtime (look for flags=...(runtime)):
codesign -dv --verbose=4 bemtvi

# Gatekeeper assessment — "accepted" / "source=Notarized Developer ID" (needs network):
spctl -a -t exec -vv bemtvi
```

The macOS TUI ships as a Developer ID Installer `.pkg` (installing to `/usr/local/bin`)
and the GUI as a `.app` inside a `.dmg` — both notarized and **stapled**, so Gatekeeper
verifies a download offline. The installed `bemtvi` binary itself carries the
hardened-runtime Developer ID signature the `codesign` check above confirms.
