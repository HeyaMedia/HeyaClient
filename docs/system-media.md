# System media integration

Heya remains the source of truth for the queue, playback state, and media
metadata. HeyaClient projects that state into the operating system and sends
semantic media commands back to Heya. The remote page never receives generic
Tauri IPC, a local artwork path, or arbitrary native APIs.

## Native surfaces

HeyaClient publishes the current title, artist, album, artwork, duration,
position, and playing state to the platform media surface. It accepts only
play, pause, play/pause, previous, next, stop, and seek commands. Stop is
treated as pause by Heya so an OS control cannot unexpectedly clear the queue.

The platform adapters are:

- macOS Now Playing and Remote Command Center.
- Windows System Media Transport Controls.
- Linux MPRIS when HeyaClient is built for Linux.

The application Playback menu provides app-local fallbacks for play/pause,
previous, and next. Hardware media keys are handled by the native platform
media session and do not require a process-wide keyboard hook.

## Track notifications

Track-change notifications are disabled by default. They can be enabled in the
local HeyaClient settings opened with Cmd/Ctrl+,. A notification is considered
only after playback advances to a different item, and is shown only while the
main window is unfocused. The native side derives its title and body from the
validated current snapshot rather than accepting notification copy from the
remote page.

## Bridge and ownership

The selected saved Heya origin receives the frozen
`window.__HEYA_SYSTEM_MEDIA__` protocol-v1 object. Every request validates the
live main-frame origin, main window label, protocol version, owner page
instance, monotonically increasing revision, payload size, and bounded text
and artwork fields. The one remote Tauri command is permissioned only for that
exact origin and window.

Artwork is fetched by the authenticated Heya frontend, reduced to a square
JPEG no larger than 512 KiB, validated again by Rust, and written into a
dedicated cache directory. Previous cached artwork is removed when the media
item changes or its owner disappears.

Commands are emitted through `heya:system-media:command-v1` only to the current
owner page. Navigation, a server switch, main-window close, or app exit clears
the ownership and native Now Playing state.

The exact public shape is documented in
[`system-media-bridge.d.ts`](system-media-bridge.d.ts).
