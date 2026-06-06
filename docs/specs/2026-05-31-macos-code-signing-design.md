# macOS Code Signing & Notarization — Design

**Date:** 2026-05-31
**Status:** Approved (pending spec review)
**Scope:** Developer ID sign + notarize the two macOS `nxvim` binaries in the release
pipeline so they pass Gatekeeper and run on any Mac.

## Goal

The macOS binaries (`x86_64-apple-darwin`, `aarch64-apple-darwin`) currently ship unsigned,
so Gatekeeper blocks them ("cannot be opened because the developer cannot be verified").
Make every published macOS binary — on **both** the rolling `edge` channel and stable `v*`
releases — **Developer ID signed with a hardened runtime and notarized by Apple**, so it runs
on any Mac.

Distribution format stays `.tar.gz` (one per arch), unchanged. The binaries are **not
stapled** — Apple does not support stapling a notarization ticket to a standalone Mach-O
executable (only `.app`/`.pkg`/`.dmg`/kext). Gatekeeper therefore verifies notarization
online. Practical consequence: a terminal install (`curl … | tar xz`) sets no quarantine and
runs offline; a browser-download + Finder-extract is quarantined and needs network on first
launch (which succeeds because the binary is notarized).

## Decisions (locked)

- **Auth:** App Store Connect API key (`.p8` + Key ID + Issuer ID). No Team ID needed — API
  key notarization doesn't use it, and `codesign` gets the team from the certificate.
- **Tooling:** Apple's official first-party CLIs invoked directly — `security`, `codesign`,
  `xcrun notarytool`. No third-party GitHub Action handles the signing material. (There is no
  official Apple-published GitHub Action; the `apple-actions` org is community, not Apple.)
- **Scope:** Sign + notarize on both `edge` (every `main` push) and stable releases.
- **Format:** Keep per-arch `.tar.gz`, notarized but unstapled.
- **Entitlements:** None. A static Rust binary with vendored Lua 5.1 (no JIT, no external
  dylib loading) runs under the default hardened runtime without entitlements.

## Architecture

All signing happens in the **macOS branch of the reusable `build.yml`**, gated
`if: matrix.os == 'macos'`, inserted **after the `Build` step and before `Package (Unix)`**.

```
build.yml (matrix job)
  ...
  Build (cargo build --release)
  [macOS only] Import signing certificate   ← new
  [macOS only] Codesign binary              ← new
  [macOS only] Notarize binary              ← new
  Package (Unix)  tar -czf  (now archives the SIGNED binary)
  Package (Windows)
  Upload artifact
```

Because both `edge.yml` and `release.yml` call `build.yml`, both channels get signed binaries
with **no new workflow inputs**. Linux and Windows steps are untouched (the signing steps are
`if: matrix.os == 'macos'`).

**Checksums & attestation ordering is already correct.** The publish jobs compute
`SHA256SUMS` and run `attest-build-provenance` over the final `.tar.gz` archives *after*
packaging. Since signing+notarization mutate the binary *before* packaging, the archives —
and therefore the checksums and attestations — cover the signed binary. No reordering needed.

## Secrets (maintainer setup — precondition)

Five repository **Actions secrets** must exist **before the next `main` push**, or the macOS
build jobs will fail at the signing step (intentionally loud — we do not ship silently
unsigned macOS binaries):

| Secret | Contents |
| --- | --- |
| `MACOS_CERT_P12` | base64 of the **Developer ID Application** certificate + private key, exported from Keychain Access as a `.p12` |
| `MACOS_CERT_PASSWORD` | the password set on that `.p12` export |
| `AC_API_KEY_P8` | base64 of the App Store Connect API key file (`AuthKey_XXXX.p8`) |
| `AC_API_KEY_ID` | the API key's **Key ID** |
| `AC_API_ISSUER_ID` | the App Store Connect API **Issuer ID** |

How to produce them:
- **Cert:** In Keychain Access, find the "Developer ID Application: … (TEAMID)" identity,
  expand it, select both the certificate and its private key, Export → `.p12` with a password.
  Then `base64 -i cert.p12 | pbcopy` → paste into `MACOS_CERT_P12`.
- **API key:** App Store Connect → Users and Access → Integrations → App Store Connect API →
  generate a **Team key** with at least **Developer** access (sufficient for notarization).
  Download the `.p8` once; `base64 -i AuthKey_XXXX.p8 | pbcopy` → `AC_API_KEY_P8`. Copy the
  Key ID and the Issuer ID shown on that page.

## Signing steps (detailed)

All three steps run only on macOS runners (`if: matrix.os == 'macos'`). Secrets are passed via
each step's `env:` block (never interpolated into the script body, never echoed).

### Step 1 — Import signing certificate

Creates a throwaway keychain in `$RUNNER_TEMP` (discarded when the ephemeral runner is torn
down — no cleanup required), imports the `.p12`, authorizes `codesign` to use the key, and
auto-detects the Developer ID Application identity (no identity secret needed).

```bash
set -euo pipefail
KEYCHAIN="$RUNNER_TEMP/signing.keychain-db"
KEYCHAIN_PW="$(openssl rand -base64 24)"
CERT="$RUNNER_TEMP/cert.p12"

echo "$MACOS_CERT_P12" | base64 --decode > "$CERT"

security create-keychain -p "$KEYCHAIN_PW" "$KEYCHAIN"
security set-keychain-settings -lut 21600 "$KEYCHAIN"     # auto-lock after 6h, no UI
security unlock-keychain -p "$KEYCHAIN_PW" "$KEYCHAIN"
security import "$CERT" -k "$KEYCHAIN" -P "$MACOS_CERT_PASSWORD" -T /usr/bin/codesign
security set-key-partition-list -S apple-tool:,apple:,codesign: -s -k "$KEYCHAIN_PW" "$KEYCHAIN" >/dev/null
# Make the temp keychain searchable alongside the existing ones.
security list-keychains -d user -s "$KEYCHAIN" $(security list-keychains -d user | sed 's/[\"[:space:]]//g')

IDENTITY="$(security find-identity -v -p codesigning "$KEYCHAIN" | awk '/Developer ID Application/ {print $2; exit}')"
if [ -z "$IDENTITY" ]; then echo "::error::No Developer ID Application identity found in keychain"; exit 1; fi
echo "SIGN_IDENTITY=$IDENTITY" >> "$GITHUB_ENV"
echo "SIGN_KEYCHAIN=$KEYCHAIN" >> "$GITHUB_ENV"
rm -f "$CERT"
```

`env:` → `MACOS_CERT_P12`, `MACOS_CERT_PASSWORD`.

### Step 2 — Codesign

```bash
set -euo pipefail
BIN="target/${{ matrix.target }}/release/nxvim"
codesign --force --options runtime --timestamp --keychain "$SIGN_KEYCHAIN" --sign "$SIGN_IDENTITY" "$BIN"
codesign --verify --strict --verbose=2 "$BIN"
```

`--options runtime` (hardened runtime) and `--timestamp` (secure timestamp) are both required
for notarization. `--force` re-signs idempotently if a cache ever carried a prior signature.

### Step 3 — Notarize

```bash
set -euo pipefail
BIN="target/${{ matrix.target }}/release/nxvim"
KEY="$RUNNER_TEMP/ac_api.p8"
ZIP="$RUNNER_TEMP/nxvim-notarize.zip"

echo "$AC_API_KEY_P8" | base64 --decode > "$KEY"
ditto -c -k --keepParent "$BIN" "$ZIP"        # notarytool needs a zip/pkg/dmg, not a bare file

if ! OUT="$(xcrun notarytool submit "$ZIP" --key "$KEY" --key-id "$AC_API_KEY_ID" --issuer "$AC_API_ISSUER_ID" --wait --timeout 30m 2>&1)"; then
  echo "$OUT"
  ID="$(printf '%s\n' "$OUT" | awk '/id:/{print $2; exit}')"
  [ -n "$ID" ] && xcrun notarytool log "$ID" --key "$KEY" --key-id "$AC_API_KEY_ID" --issuer "$AC_API_ISSUER_ID" || true
  rm -f "$KEY"
  exit 1
fi
printf '%s\n' "$OUT"
rm -f "$KEY"
```

`env:` → `AC_API_KEY_P8`, `AC_API_KEY_ID`, `AC_API_ISSUER_ID`. `--wait` blocks until Apple
finishes and returns non-zero on rejection (failing the job); on failure we fetch and print
the notary log for diagnostics. No `stapler` step — impossible for a bare executable.

The subsequent existing `Package (Unix)` step then `tar -czf`s the signed binary unchanged.

## Error handling & edge cases

- **Missing secrets:** the import step fails loudly. By design — we never ship silently
  unsigned macOS binaries. (Documented as a precondition.)
- **Notarization rejected:** `notarytool --wait` exits non-zero → job fails; the notary log is
  printed so the rejection reason (e.g. an entitlement or hardened-runtime issue) is visible.
- **Multiple Developer ID Application identities** in the cert bundle: the `awk` picks the
  first. For a single-cert team this is unambiguous; noted as a minor caveat.
- **Edge `cancel-in-progress`:** a superseded run may abandon an in-flight notary submission;
  Apple still completes it server-side, and the next `main` push re-submits. Harmless.
- **Keychain cleanup:** none needed — GitHub macOS runners are ephemeral and destroyed after
  the job.
- **Secret exposure:** `build.yml` is only ever called by `edge.yml` (push to `main`) and
  `release.yml` (merged labeled PR) — both trusted contexts. It is never invoked from an
  untrusted fork PR, so the signing secrets are never exposed to untrusted code.

## Docs

Extend `docs/verifying-downloads.md` with a macOS section: the binaries are Developer
ID-signed and notarized; users can confirm with `codesign --verify --verbose nxvim` and
`spctl -a -t exec -vv nxvim` (the latter requires network for the unstapled online check).

## Testing strategy

CI configuration — validated by running it, not the Rust test harness:

1. Add the five secrets, push to `main`, and confirm both macOS build jobs run the Import →
   Codesign → Notarize steps and that `notarytool` reports `status: Accepted`.
2. Download a published macOS archive and verify locally:
   - `codesign -dv --verbose=4 nxvim` shows the Developer ID authority + hardened runtime
     (`flags=0x10000(runtime)`) + a secure timestamp.
   - `spctl -a -t exec -vv nxvim` reports `accepted` / `source=Notarized Developer ID`.
   - `xcrun notarytool history --key … ` lists the submission as Accepted.
3. Confirm Linux/Windows jobs are unaffected and still green.
4. Confirm the existing `SHA256SUMS` + provenance attestation still pass over the now-signed
   archives.

## Risks / to verify during implementation

- **Hardened runtime without entitlements:** expected to pass for a static Rust binary, but
  only the first real notarization confirms it. Fallback if Apple flags it: add a minimal
  entitlements plist (e.g. `com.apple.security.cs.disable-library-validation`) and pass
  `--entitlements` to `codesign`.
- **API key role:** the App Store Connect API key must have sufficient access for
  notarization; if `notarytool` returns an auth error, regenerate the key with Developer (or
  higher) access.
- **`security set-key-partition-list`** is the historically finicky step; the recipe above is
  the established one for non-interactive CI signing.
```
