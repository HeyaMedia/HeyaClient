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

The `native-mpv` release feature links Heya's own shim rather than libmpv. On
macOS it resolves an allowlisted API table with `dlopen`; on Windows it uses
`LoadLibraryExW` with a narrowly scoped dependency search. It caches successful
loads but retries failures, so installing MPV and pressing **Check again**
works without restarting HeyaClient. The existing `PlaybackEngine` boundary
remains authoritative; the remote Heya page never receives library paths,
download URLs, raw MPV commands, or install controls.

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

1. Discover an installed libmpv through Homebrew's stable prefixes at
   `/opt/homebrew` and `/usr/local`, with MacPorts and conventional local
   library prefixes as fallbacks.
2. Validate compatibility by creating and initializing a probe instance.
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

This milestone is implemented. The runtime lives below HeyaClient's per-user
local app-data directory and is never copied into the application installation
directory or release artifact. `LoadLibraryExW` searches only the verified DLL
directory and `System32` for dependencies; it does not search the current
directory, `PATH`, or arbitrary package-manager locations.

Failed or cancelled installations remove the temporary directory. Updates are
explicit user actions and install beside the previous version before switching
the active receipt. Removal deletes only HeyaClient-managed Windows files.

## Plugins

MPV starts with normal global configuration, script autoloading, and its
built-in Lua console/stats/select/auto-profile tools disabled. Heya owns those
controls and diagnostics, and avoiding the unused LuaJIT tools keeps executable
JIT pages out of the hardened macOS process.
HeyaClient loads only plugins explicitly enabled from its own native-plugin
directory. A plugin is trusted native playback code: it can affect playback and
may invoke powerful MPV facilities. The settings UI must show its origin and
require explicit enablement. The remote Heya page cannot select plugin paths or
install plugins.

## Implementation order

1. Finish and test the renderer behind `PlaybackEngine`. **Done.**
2. Replace macOS direct linkage with a refreshable runtime loader. **Done.**
3. Add native runtime status commands restricted to the local settings window.
   **Done.**
4. Add macOS discovery and Settings retry. **Done.**
5. Implement the explicit Homebrew install action and progress UI.
6. Implement the verified Windows archive installer. **Done.** Clean-machine
   hardware validation remains required.
7. Add plugin discovery and explicit enablement after playback itself is stable.
8. Design Linux package-manager/Flatpak behavior separately.

Every macOS and Windows CI release runs `scripts/verify-no-bundled-libmpv.py`
against the native-capable application binary to prove it contains no bundled
or load-time MPV runtime.

## Windows hardware validation

The normal release is the test artifact. It must launch without MPV, offer the
local verified install action, activate native playback without an application
restart, and continue to fall back to browser playback when installation is
declined or fails. The real-hardware checklist lives in
[`windows-testing.md`](windows-testing.md).
