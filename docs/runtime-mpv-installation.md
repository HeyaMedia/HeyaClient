# Optional MPV runtime installation

HeyaClient is MIT-licensed and its ordinary build contains no MPV binary. The
browser renderer is always available. Native playback becomes available only
after the user explicitly installs MPV from the local HeyaClient settings
window.

This document defines the product and engineering boundary. It does not claim
that a user-initiated download changes any third-party license.

## Non-negotiable runtime boundary

The main HeyaClient process must start, open settings, connect to Heya, and use
browser playback on a machine with no MPV installation. Therefore the release
binary must not have a load-time dependency on libmpv.

The in-progress renderer may continue using the current `native-mpv` Cargo
feature for development. Once its rendering and lifecycle behavior are proven,
its direct libmpv calls move behind a runtime-loaded API table using
`dlopen`/`LoadLibrary`, or into an optional native helper. The existing
`PlaybackEngine` boundary remains authoritative; the remote Heya page never
receives library paths, download URLs, raw MPV commands, or install controls.

The current one-shot libmpv availability cache must also become refreshable.
Installing MPV after an initial failed probe must immediately make a new probe
possible without restarting HeyaClient.

## Provider manifest

[`providers-v1.json`](../src-tauri/native/mpv/providers-v1.json) is compiled
into HeyaClient. It is not supplied by the remote Heya server. Provider updates
are ordinary reviewed HeyaClient changes, so fifty application builds can reuse
one pinned MPV provider revision without redownloading or repackaging it.

The installer accepts no caller-supplied URL, checksum, archive path, command,
package name, or library path. It selects everything from the compiled
manifest and the native target architecture.

## Installation state

The native service exposes a small state machine to the local settings page:

- `not_installed`
- `discovering`
- `available`
- `installing`
- `failed`
- `unsupported`

An `available` result includes the provider, detected MPV version, architecture,
and native capabilities, but no filesystem path. Installation progress is sent
only to the local settings WebView. The selected remote Heya origin can query
playback capabilities through the existing narrow bridge, but cannot trigger
installation, updates, removal, or arbitrary process execution.

## macOS milestone

1. Look for Homebrew only at `/opt/homebrew/bin/brew` and
   `/usr/local/bin/brew`.
2. Ask Homebrew for the installed MPV prefix and validate libmpv plus its
   version.
3. If absent, show an explicit confirmation for `brew install mpv`.
4. Run that exact command without a shell and stream sanitized progress to the
   local settings page.
5. Re-probe through the runtime loader after Homebrew exits successfully.
6. Never install Homebrew, use `sudo`, modify shell profiles, or uninstall an
   MPV installation owned by the user.

If Homebrew itself is absent, the action opens the Homebrew/MPV installation
page and browser playback remains selected.

## Windows milestone

1. Select the x86-64 or ARM64 asset from the compiled provider manifest.
2. Confirm the provider, version, download size, and destination with the user.
3. Download over HTTPS with redirects restricted to GitHub's expected hosts and
   enforce the declared maximum size.
4. Verify SHA-256 before parsing the archive.
5. Extract only the expected regular files into a temporary application-data
   directory; reject absolute paths, parent traversal, links, and duplicate
   entries.
6. Validate `libmpv-2.dll`, write an installation receipt, and atomically rename
   the directory into place.
7. Load the DLL through the runtime API and re-probe capabilities.

Failed or cancelled installations remove the temporary directory. Updates are
explicit user actions and install beside the previous version before switching
the active receipt. Removal deletes only HeyaClient-managed Windows files.

## Plugins

MPV starts with normal global configuration and script autoloading disabled.
HeyaClient loads only plugins explicitly enabled from its own native-plugin
directory. A plugin is trusted native playback code: it can affect playback and
may invoke powerful MPV facilities. The settings UI must show its origin and
require explicit enablement. The remote Heya page cannot select plugin paths or
install plugins.

## Implementation order

1. Finish and test the renderer using the existing development-only direct
   libmpv feature.
2. Preserve the renderer behind `PlaybackEngine` while replacing direct linkage
   with a refreshable runtime loader.
3. Add native runtime status commands restricted to the local settings window.
4. Implement macOS discovery and the explicit Homebrew action.
5. Add settings UI for status, installation, failure, retry, and browser
   fallback.
6. Implement the verified Windows archive installer and test clean-machine
   installation, update, rollback, and removal.
7. Add plugin discovery and explicit enablement after playback itself is stable.
8. Design Linux package-manager/Flatpak behavior separately.

Every ordinary CI and release build must run
`scripts/verify-no-bundled-libmpv.py` against the default application bundle.

## Windows hardware-test preview

Until the runtime loader and user-initiated installer exist, CI may produce a
private, explicitly named Windows development preview containing the pinned
provider DLL beside a debug Heya executable. This is not a normal release or
installer. Its only purpose is real-hardware validation of the directly linked
adapter, and it must include the provider receipt/source reference plus the
tester boundary in `docs/windows-testing.md`.

The ordinary Windows installer is built and verified separately and must still
launch without MPV. Passing the preview tests does not satisfy the production
runtime-install milestone.
