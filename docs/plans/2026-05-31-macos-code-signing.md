# macOS Code Signing & Notarization Implementation Plan

**Goal:** Developer ID sign (hardened runtime) + notarize the two macOS `bemtvi` binaries in the reusable build workflow, so every macOS binary on both the `edge` and stable channels runs on any Mac.

**Architecture:** Three macOS-only steps (`if: matrix.os == 'macos'`) are inserted into `.github/workflows/build.yml` between the `Build` and `Package (Unix)` steps — import cert into a throwaway keychain, `codesign`, then `xcrun notarytool submit --wait`. Because `build.yml` is a reusable (`workflow_call`) workflow, the five signing secrets are declared in its `workflow_call.secrets` block and the two callers (`edge.yml`, `release.yml`) pass them with `secrets: inherit` — otherwise `${{ secrets.* }}` is empty inside `build.yml`. Signing mutates the binary before `tar`, so the existing checksums + provenance attestation (computed in the publish jobs over the archives) already cover the signed binary.

**Tech Stack:** Apple first-party CLIs — `security`, `codesign`, `xcrun notarytool` — with App Store Connect API-key auth. No third-party actions touch the signing material.

**Reference spec:** `docs/specs/2026-05-31-macos-code-signing-design.md`

**Local validation tool:** `actionlint` (already installed) validates the workflow YAML and runs shellcheck on the `run:` scripts.

---

## File Structure

| File | Change |
| --- | --- |
| `.github/workflows/build.yml` | Declare 5 signing secrets under `workflow_call.secrets`; add 3 macOS-only steps (import cert → codesign → notarize) between `Build` and `Package (Unix)`. |
| `.github/workflows/edge.yml` | Add `secrets: inherit` to the `build` job. |
| `.github/workflows/release.yml` | Add `secrets: inherit` to the `build` job. |
| `docs/verifying-downloads.md` | Add a "macOS signature & notarization" section. |

No other files change. Linux/Windows build steps, `release-prep.yml`, `cliff.toml`, and `Cargo.toml` are untouched.

---

## Task 0: Preconditions (maintainer — create 5 secrets)

One-time setup in the GitHub UI + locally. No repo code. **Must be done before the next `main` push**, or the macOS build jobs fail at the signing step (intentional — we never ship silently unsigned macOS binaries).

- [ ] **Step 1: Export the Developer ID Application certificate as base64**

In **Keychain Access**, locate the `Developer ID Application: … (TEAMID)` identity, expand it, select **both** the certificate and its private key → right-click → Export 2 items → save as `cert.p12` with a password. Then locally:

```bash
base64 -i cert.p12 | pbcopy
```

Create repo secret **`MACOS_CERT_P12`** (Settings → Secrets and variables → Actions → New repository secret) with the clipboard contents, and **`MACOS_CERT_PASSWORD`** with the `.p12` export password.

- [ ] **Step 2: Create an App Store Connect API key**

App Store Connect → **Users and Access → Integrations → App Store Connect API** → generate a **Team key** with at least **Developer** access. Download `AuthKey_XXXXXXXXXX.p8` (one-time download). Note the **Key ID** and the **Issuer ID** shown on that page. Then locally:

```bash
base64 -i AuthKey_XXXXXXXXXX.p8 | pbcopy
```

Create secrets **`AC_API_KEY_P8`** (clipboard), **`AC_API_KEY_ID`** (the Key ID), **`AC_API_ISSUER_ID`** (the Issuer ID).

- [ ] **Step 3: Confirm all five secrets exist**

Settings → Secrets and variables → Actions should list exactly: `MACOS_CERT_P12`, `MACOS_CERT_PASSWORD`, `AC_API_KEY_P8`, `AC_API_KEY_ID`, `AC_API_ISSUER_ID` (alongside the existing none-required ones).

No commit for this task.

---

## Task 1: Add macOS signing steps to `build.yml`

**Files:**
- Modify: `.github/workflows/build.yml` (declare secrets; insert 3 steps between `Build` and `Package (Unix)`)

- [ ] **Step 1: Declare the signing secrets on `workflow_call`**

`build.yml` is a reusable workflow. Secrets referenced inside it must be declared on its
`workflow_call` trigger (and passed by callers — Task 2). Find this exact block at the top of
`.github/workflows/build.yml`:

```yaml
on:
  workflow_call:
    inputs:
      version:
        description: "Version string used in archive names (e.g. 0.2.0 or edge)"
        required: true
        type: string
```

Replace it with (adds a `secrets:` section; `required: false` so the workflow still parses for
non-signing reuse, with a loud runtime failure if a macOS job runs without them):

```yaml
on:
  workflow_call:
    inputs:
      version:
        description: "Version string used in archive names (e.g. 0.2.0 or edge)"
        required: true
        type: string
    secrets:
      MACOS_CERT_P12:
        required: false
      MACOS_CERT_PASSWORD:
        required: false
      AC_API_KEY_P8:
        required: false
      AC_API_KEY_ID:
        required: false
      AC_API_ISSUER_ID:
        required: false
```

- [ ] **Step 2: Insert the signing steps**

In `.github/workflows/build.yml`, find this exact block:

```yaml
      - name: Build
        shell: bash
        run: cargo build --release -p bemtvi --target ${{ matrix.target }}

      - name: Package (Unix)
        if: matrix.os != 'windows'
        shell: bash
        run: |
          mkdir -p dist
          tar -czf "dist/$ASSET" -C "target/${{ matrix.target }}/release" bemtvi
```

Replace it with this block (the `Build` and `Package (Unix)` steps are unchanged; three new macOS-only steps are inserted between them):

```yaml
      - name: Build
        shell: bash
        run: cargo build --release -p bemtvi --target ${{ matrix.target }}

      - name: Import signing certificate (macOS)
        if: matrix.os == 'macos'
        shell: bash
        env:
          MACOS_CERT_P12: ${{ secrets.MACOS_CERT_P12 }}
          MACOS_CERT_PASSWORD: ${{ secrets.MACOS_CERT_PASSWORD }}
        run: |
          set -euo pipefail
          KEYCHAIN="$RUNNER_TEMP/signing.keychain-db"
          KEYCHAIN_PW="$(openssl rand -base64 24)"
          CERT="$RUNNER_TEMP/cert.p12"

          echo "$MACOS_CERT_P12" | base64 --decode > "$CERT"

          security create-keychain -p "$KEYCHAIN_PW" "$KEYCHAIN"
          security set-keychain-settings -lut 21600 "$KEYCHAIN"
          security unlock-keychain -p "$KEYCHAIN_PW" "$KEYCHAIN"
          security import "$CERT" -k "$KEYCHAIN" -P "$MACOS_CERT_PASSWORD" -T /usr/bin/codesign
          security set-key-partition-list -S apple-tool:,apple:,codesign: -s -k "$KEYCHAIN_PW" "$KEYCHAIN" >/dev/null
          # Make the temp keychain searchable alongside the existing ones (word-splitting intended).
          # shellcheck disable=SC2046
          security list-keychains -d user -s "$KEYCHAIN" $(security list-keychains -d user | sed 's/[\"[:space:]]//g')

          IDENTITY="$(security find-identity -v -p codesigning "$KEYCHAIN" | awk '/Developer ID Application/ {print $2; exit}')"
          if [ -z "$IDENTITY" ]; then
            echo "::error::No Developer ID Application identity found in the imported certificate"
            exit 1
          fi
          echo "SIGN_IDENTITY=$IDENTITY" >> "$GITHUB_ENV"
          echo "SIGN_KEYCHAIN=$KEYCHAIN" >> "$GITHUB_ENV"
          rm -f "$CERT"

      - name: Codesign binary (macOS)
        if: matrix.os == 'macos'
        shell: bash
        run: |
          # shellcheck disable=SC2154  # SIGN_KEYCHAIN/SIGN_IDENTITY arrive via $GITHUB_ENV
          set -euo pipefail
          BIN="target/${{ matrix.target }}/release/bemtvi"
          codesign --force --options runtime --timestamp --keychain "$SIGN_KEYCHAIN" --sign "$SIGN_IDENTITY" "$BIN"
          codesign --verify --strict --verbose=2 "$BIN"

      - name: Notarize binary (macOS)
        if: matrix.os == 'macos'
        shell: bash
        env:
          AC_API_KEY_P8: ${{ secrets.AC_API_KEY_P8 }}
          AC_API_KEY_ID: ${{ secrets.AC_API_KEY_ID }}
          AC_API_ISSUER_ID: ${{ secrets.AC_API_ISSUER_ID }}
        run: |
          set -euo pipefail
          BIN="target/${{ matrix.target }}/release/bemtvi"
          KEY="$RUNNER_TEMP/ac_api.p8"
          ZIP="$RUNNER_TEMP/bemtvi-notarize.zip"

          echo "$AC_API_KEY_P8" | base64 --decode > "$KEY"
          ditto -c -k --keepParent "$BIN" "$ZIP"

          if ! OUT="$(xcrun notarytool submit "$ZIP" --key "$KEY" --key-id "$AC_API_KEY_ID" --issuer "$AC_API_ISSUER_ID" --wait --timeout 30m 2>&1)"; then
            printf '%s\n' "$OUT"
            ID="$(printf '%s\n' "$OUT" | awk '/id:/{print $2; exit}')"
            if [ -n "$ID" ]; then
              xcrun notarytool log "$ID" --key "$KEY" --key-id "$AC_API_KEY_ID" --issuer "$AC_API_ISSUER_ID" || true
            fi
            rm -f "$KEY"
            exit 1
          fi
          printf '%s\n' "$OUT"
          rm -f "$KEY"

      - name: Package (Unix)
        if: matrix.os != 'windows'
        shell: bash
        run: |
          mkdir -p dist
          tar -czf "dist/$ASSET" -C "target/${{ matrix.target }}/release" bemtvi
```

- [ ] **Step 3: Validate with actionlint**

Run: `actionlint .github/workflows/build.yml`
Expected: exit 0, no output. actionlint runs shellcheck on the new `run:` blocks — the `# shellcheck disable=SC2046` directive suppresses the intentional word-splitting warning on the `security list-keychains` line, and the file-level `# shellcheck disable=SC2154` in the codesign step covers `SIGN_KEYCHAIN`/`SIGN_IDENTITY` (set via `$GITHUB_ENV`). If a *different* shellcheck warning or YAML error appears, report it exactly (do not redesign the steps).

- [ ] **Step 4: Confirm the rest of the workflow is unchanged**

Run: `git diff .github/workflows/build.yml`
Expected: only additions — the `secrets:` block and the three new steps. The `Build`, `Package (Unix)`, `Package (Windows)`, and `Upload artifact` steps are byte-for-byte unchanged.

- [ ] **Step 5: Commit**

```bash
git add .github/workflows/build.yml
git commit -m "ci: sign and notarize macOS binaries"
```

(The pre-commit hook runs cargo fmt/clippy — it skips, no Rust changed.)

---

## Task 2: Pass secrets from the calling workflows

The reusable `build.yml` only receives secrets if its callers pass them. Add `secrets: inherit`
to the `build` job in both callers.

**Files:**
- Modify: `.github/workflows/edge.yml` (the `build` job)
- Modify: `.github/workflows/release.yml` (the `build` job)

- [ ] **Step 1: Update `edge.yml`**

In `.github/workflows/edge.yml`, find this exact block:

```yaml
jobs:
  build:
    uses: ./.github/workflows/build.yml
    with:
      version: edge
```

Replace it with:

```yaml
jobs:
  build:
    uses: ./.github/workflows/build.yml
    with:
      version: edge
    secrets: inherit
```

- [ ] **Step 2: Update `release.yml`**

In `.github/workflows/release.yml`, find this exact block:

```yaml
  build:
    needs: tag
    uses: ./.github/workflows/build.yml
    with:
      version: ${{ needs.tag.outputs.version }}
```

Replace it with:

```yaml
  build:
    needs: tag
    uses: ./.github/workflows/build.yml
    with:
      version: ${{ needs.tag.outputs.version }}
    secrets: inherit
```

- [ ] **Step 3: Validate with actionlint**

Run: `actionlint .github/workflows/edge.yml .github/workflows/release.yml`
Expected: exit 0, no output. If an error appears, report it exactly.

- [ ] **Step 4: Commit**

```bash
git add .github/workflows/edge.yml .github/workflows/release.yml
git commit -m "ci: pass signing secrets to the reusable build workflow"
```

---

## Task 3: Document macOS verification

**Files:**
- Modify: `docs/verifying-downloads.md` (append a macOS section)

- [ ] **Step 1: Append the macOS section**

Add the following to the end of `docs/verifying-downloads.md` (after the existing "Provenance attestation" section):

```markdown

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

The binaries are not stapled (Apple does not support stapling a notarization ticket to a
standalone executable), so the `spctl` check performs an online verification. A terminal
install (`curl … | tar xz`) sets no quarantine attribute and runs offline regardless.
```

- [ ] **Step 2: Confirm the nested code fences render**

Run: `cat docs/verifying-downloads.md`
Expected: the new section appears after "Provenance attestation"; the inner ` ```sh ` block is intact inside the document.

- [ ] **Step 3: Commit**

```bash
git add docs/verifying-downloads.md
git commit -m "docs: how to verify macOS signature and notarization"
```

---

## Task 4: End-to-end verification on GitHub

Exercises signing on real runners. Requires Task 0's secrets and the Task 1–2 changes on `main`.

- [ ] **Step 1: Get the change onto `main`**

Merge Tasks 1–3 to `main` (normal flow). The push triggers an `edge` run.

- [ ] **Step 2: Confirm both macOS jobs sign + notarize**

In the Actions tab, open the triggered `edge` run. Expected: `build / x86_64-apple-darwin` and `build / aarch64-apple-darwin` each run **Import signing certificate**, **Codesign binary**, and **Notarize binary** successfully, with the notarize step's log showing `status: Accepted`. Linux/Windows jobs and the `publish` job (checksums + attestation) stay green.

- [ ] **Step 3: Verify a downloaded binary locally**

```bash
gh release download edge --repo bemtvi/bemtvi --pattern 'bemtvi-edge-aarch64-macos.tar.gz' --dir /tmp
tar -xzf /tmp/bemtvi-edge-aarch64-macos.tar.gz -C /tmp
codesign -dv --verbose=4 /tmp/bemtvi 2>&1 | grep -E 'Authority=Developer ID Application|flags=.*runtime|Timestamp='
spctl -a -t exec -vv /tmp/bemtvi
```
Expected: `codesign` shows a `Developer ID Application` authority, a `runtime` flag, and a secure `Timestamp`; `spctl` reports `accepted` with `source=Notarized Developer ID`. (Run on an Apple-Silicon Mac for the aarch64 artifact, or download the `x86_64` artifact on an Intel Mac.)

- [ ] **Step 4: Confirm the attestation still verifies over the signed archive**

```bash
gh attestation verify /tmp/bemtvi-edge-aarch64-macos.tar.gz --repo bemtvi/bemtvi
```
Expected: `✓ Verification succeeded`.

No commit for this task (verification only). If notarization is rejected for a hardened-runtime/entitlement reason (visible in the printed notary log), add a minimal entitlements plist and pass `--entitlements` to the `codesign` step — see the spec's "Risks" section.

---

## Notes for the implementer

- **No unit tests.** This is CI configuration; the project's integration-test harness does not apply. Validation is `actionlint` locally + the live run in Task 4. Do not add `#[test]` code.
- **Secrets are referenced only via step `env:`** and decoded to `$RUNNER_TEMP` files that are `rm`'d after use; never echo them. The temp keychain needs no cleanup (ephemeral runner).
- **Ordering matters:** the three steps go *after* `Build` and *before* `Package (Unix)`, so the `tar` archives the signed binary. Do not move checksum/attestation logic — it lives in the publish jobs and already runs over the final archives.
- **Both macOS runners** (`macos-15-intel`, `macos-14`) ship Xcode with `notarytool`; no toolchain install needed.
```
