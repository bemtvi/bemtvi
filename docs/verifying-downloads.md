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
gh attestation verify nxvim-0.2.0-x86_64-linux-musl.tar.gz --repo davidrios/nxvim
```

A successful run confirms the artifact was produced by the `nxvim` release
workflow at a specific commit, and was not tampered with afterwards.
