# Verifying downloads

Every released `nxvim` binary ships with a SHA-256 checksum and a signed
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
gh attestation verify nxvim-0.2.0-x86_64-linux.tar.gz --repo davidrios/nxvim
```

A successful run confirms the artifact was produced by the `nxvim` release
workflow at a specific commit, and was not tampered with afterwards.

## macOS signature & notarization

The macOS binaries are signed with an Apple **Developer ID Application** certificate, built
with the hardened runtime, and **notarized** by Apple, so they run on any Mac without a
Gatekeeper override. Confirm locally:

```sh
# Signature, authority chain, hardened runtime (look for flags=...(runtime)):
codesign -dv --verbose=4 nxvim

# Gatekeeper assessment — "accepted" / "source=Notarized Developer ID" (needs network):
spctl -a -t exec -vv nxvim
```

The TUI binary is not stapled (Apple does not support stapling a notarization ticket to a
standalone executable), so the `spctl` check performs an online verification. A terminal
install (`curl … | tar xz`) sets no quarantine attribute and runs offline regardless.
