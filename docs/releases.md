# HeyaClient releases and updates

HeyaClient checks the latest GitHub Release once at startup. When a newer
signed version is available, the native settings window opens and lets the
user approve the download and installation. The remote Heya WebView is not
given updater permissions.

## Release contract

- Application versions in `package.json`, `src-tauri/Cargo.toml`, and
  `src-tauri/tauri.conf.json` must match.
- A release tag must be that version prefixed with `v`, for example `v0.2.0`.
- Pull requests and pushes to `main` run checks only.
- Tags build macOS ARM64 and Windows x64 installers, sign their Tauri updater
  artifacts, create `latest.json`, and publish all of them in a GitHub Release.
- `v0.2.0` is the first updater-capable baseline. Earlier builds cannot update
  themselves automatically.

The configured feed is:

`https://github.com/HeyaMedia/HeyaClient/releases/latest/download/latest.json`

GitHub does not serve private release assets anonymously. Installed clients
can use that feed once the repository and its releases are public; while the
repository is private, releases remain available for direct authenticated
testing only.

## Signing key

Tauri updater signatures are separate from Apple code signing and Windows
Authenticode. The application contains only the updater public key. GitHub
Actions must define this repository secret:

- `TAURI_SIGNING_PRIVATE_KEY`: the complete minisign-compatible private key.

The current key has an empty password, which the release workflow supplies
explicitly. Keep the private key outside the repository and backed up. Losing
it means existing installations cannot trust future updates without a manual
replacement install.

For a local signed macOS build, load the private key into
`TAURI_SIGNING_PRIVATE_KEY`, set `TAURI_SIGNING_PRIVATE_KEY_PASSWORD` to the
key password (an empty value for the current key), and run:

```sh
bun tauri build --bundles app,dmg
```

The expected updater outputs are `Heya.app.tar.gz` and
`Heya.app.tar.gz.sig`. Windows tag builds produce an NSIS `.exe` and matching
`.exe.sig`.

## Publishing

Before creating a tag, run:

```sh
python3 scripts/check-version-sync.py
```

The tag workflow rejects mismatched versions. The publish job also refuses to
create `latest.json` unless it finds exactly one updater artifact and matching
signature for each supported platform.
