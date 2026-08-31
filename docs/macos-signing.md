# Setting up macOS signing

Everything in the release workflow is already wired for Developer ID signing
and notarization. What it needs is six secrets in the
`github-action-releaser` environment, and only the Apple Developer **Account
Holder** can produce two of them. This is the runbook for
that person. It is a one-time setup, plus a certificate renewal every five
years.

No Mac is required. The certificate is created with `openssl` and the whole
signing pipeline runs on GitHub's macOS runners.

## What CI does with these

On a tag, `release.yml` passes the secrets to `build.yml`, which then:

1. imports the certificate into a throwaway keychain on each macOS runner,
2. signs the bundled QEMU dylibs (`fetch-qemu-macos.sh` does this, before the
   app bundle seals them, which is the order Apple requires),
3. builds with `hardenedRuntime` and the entitlements in
   `launcher/entitlements.plist`,
4. submits the `.dmg` to `notarytool`, waits, and staples both the `.dmg` and
   the `.app`,
5. fails the build unless `spctl` and `stapler validate` accept both artifacts,
   on the build runner and again on a clean runner that never held the
   certificate.

Until the secrets exist, all of that is skipped and macOS builds stay ad-hoc
signed, exactly as they were. Nothing breaks in the meantime, and pull requests
from forks keep working after the secrets are added.

## 1. Create the Developer ID Application certificate

Two constraints worth knowing before you start: for an organization account
**only the Account Holder** can create a Developer ID certificate, and an
account is limited to **five** of them. Do not create throwaway ones. If you
already have one and can still lay hands on its private key, skip to step 2.

Generate a key and a signing request:

```sh
openssl req -newkey rsa:2048 -nodes \
  -keyout devid.key -out devid.csr \
  -subj "/CN=Dark Bio AG/emailAddress=<account holder email>/C=CH"
```

Then, at [developer.apple.com](https://developer.apple.com/account/resources/certificates/list):
**Certificates, IDs & Profiles** → **Certificates** → **+** → **Developer ID
Application** → upload `devid.csr` → download `developerID_application.cer`.

Download the issuing intermediate, **Developer ID Certification Authority
(G2)**, from [apple.com/certificateauthority](https://www.apple.com/certificateauthority/).
Without it in the bundle, `codesign` on the runner cannot build a chain to
Apple's root and signing fails there rather than here.

Combine all three into a PKCS#12 file:

```sh
openssl x509 -inform DER -in developerID_application.cer -out devid.pem
openssl pkcs12 -export -legacy \
  -out devid.p12 -inkey devid.key -in devid.pem \
  -certfile DeveloperIDG2CA.pem
```

`-legacy` matters: without it OpenSSL 3 encrypts with AES-256, which the
`security import` on the runner cannot read.

Read off the exact identity string, which is what `APPLE_SIGNING_IDENTITY`
must contain verbatim:

```sh
openssl x509 -in devid.pem -noout -subject
# CN = Developer ID Application: Dark Bio AG (A1B2C3D4E5), OU = A1B2C3D4E5, ...
```

Keep `devid.key` and `devid.p12` somewhere durable and private. Losing them
means burning another of the five certificate slots.

## 2. Create an App Store Connect API key

This is what authenticates the notarization request. It is preferred over an
Apple ID plus app-specific password because it is not tied to one person's
account or their 2FA.

In [App Store Connect](https://appstoreconnect.apple.com/access/integrations/api):
**Users and Access** → **Integrations** → **App Store Connect API** → **Team
Keys** → **+**. Give it the **Developer** role.

Download the `.p8`. **It can only be downloaded once.** Note the **Key ID**
next to it and the **Issuer ID** shown above the key list.

## 3. Add the environment secrets

Under **Settings** → **Environments** → **github-action-releaser** →
**Add environment secret**:

| Secret | Value |
|---|---|
| `APPLE_CERTIFICATE` | `base64 -w0 devid.p12` (on macOS: `base64 -i devid.p12`) |
| `APPLE_CERTIFICATE_PASSWORD` | the export password you chose in step 1 |
| `APPLE_SIGNING_IDENTITY` | `Developer ID Application: Dark Bio AG (A1B2C3D4E5)` |
| `APPLE_API_KEY_P8` | `base64 -w0 AuthKey_XXXXXXXXXX.p8` |
| `APPLE_API_KEY` | the key's Key ID, e.g. `XXXXXXXXXX` |
| `APPLE_API_ISSUER` | the Issuer ID, a UUID |

`APPLE_API_KEY` and `APPLE_API_KEY_P8` are easy to fill in the wrong way round;
the workflow checks that the second one decodes to a private key and says so if
it does not.

The environment name is what turns signing on. `release.yml` passes
`signing-environment: github-action-releaser` to the build workflow along with
`secrets: inherit`, and the build job declares that environment. All three are
needed: a reusable workflow can only read environment secrets by declaring the
environment itself, and only if the caller inherited secrets to it. Renaming the
environment means editing `release.yml` to match.

Because the environment gates the secrets, any deployment protection rule on it
also gates every release build. A required reviewer will pause the macOS jobs
until someone approves; a branch or tag rule that does not admit `v*.*.*` will
make them build unsigned rather than fail, so check the rules there if a tag
comes out ad-hoc signed.

All six are read only by the macOS legs of the build, and a partial set does not
degrade gracefully: the run will sign and then fail at notarization.

## 4. Verify

Tag a release candidate and watch the run:

```sh
git tag v0.1.0-rc27 && git push origin v0.1.0-rc27
```

In the **Verify macOS signature** step of both macOS jobs, look for:

```
Authority=Developer ID Application: Dark Bio AG (A1B2C3D4E5)
Authority=Developer ID Certification Authority
Authority=Apple Root CA
...
The validate action worked!
... accepted
source=Notarized Developer ID
```

`source=Notarized Developer ID` is the line that matters. `source=Unnotarized
Developer ID` means the ticket did not staple, and anything mentioning "no
usable signature" means the certificate never imported.

Then confirm on a real Mac, ideally one Apple Silicon and one Intel, since the
two are built and notarized independently:

1. Download the `.dmg` through a browser, so it is quarantined the way a user's
   copy is. Open it, drag to Applications, launch. There should be no warning
   beyond the ordinary "downloaded from the internet" confirmation.
2. Boot a guest. This is the part CI cannot check: it proves the hardened
   runtime did not cost the app its entitlements. If the guest boots but is
   sluggish, acceleration silently fell back to TCG, which means the
   `com.apple.security.hypervisor` entitlement did not survive signing.
3. Repeat with the `.zip`.

## When notarization is rejected

`notarytool` reports only that the submission was Invalid. The reason is in the
log, which the workflow prints automatically on failure. To read it by hand:

```sh
xcrun notarytool log <submission-id> \
  --key AuthKey_XXXXXXXXXX.p8 --key-id <key id> --issuer <issuer id>
```

The likely cause is one of the QEMU pieces bundled from Homebrew, since those
are the only Mach-O files in the app that Dark Bio did not compile: a dylib
under `Contents/Resources/qemu-libs/` that arrived unsigned or without a secure
timestamp. `fetch-qemu-macos.sh` signs all of them, so a rejection there points
at a file it missed rather than at a missing setup step.

## Renewal

The certificate expires five years after issue. Signatures made before it
expires stay valid, because they carry Apple's secure timestamp, so an expired
certificate does not break already-published releases. It does break the next
one. Reissue as in step 1 and replace `APPLE_CERTIFICATE`,
`APPLE_CERTIFICATE_PASSWORD` and `APPLE_SIGNING_IDENTITY`.

API keys do not expire, but revoke and reissue if one is ever exposed. A
revoked key only stops future notarizations; it does not affect releases that
were already notarized.
