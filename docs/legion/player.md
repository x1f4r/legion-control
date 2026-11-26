# Watching and listening

Opening a video or a song from any machine should feel like a player, not like a file that
happens to play: the picture fills the screen, the controls get out of the way, music shows its
cover and knows the next track. The same player runs in the Android and iOS apps, the desktop
apps and the browser; only "fill the whole screen" is done by each platform in its own way.

This builds on `files.md`: the bytes still come from content addresses, and the viewer is still
the one in `apps/ui/views/files-viewer.js`.

## Rules

- **Legion draws the controls.** The system's own `controls` on `<video>` and `<audio>` are not
  used. They look different everywhere, cannot be hidden reliably, and their full-screen button
  does not work inside an app's web view.
- **A control that cannot work is absent.** Never disabled, never greyed out.
- **When the viewer's bars are hidden, the system's are too.** One state, for videos and
  pictures alike.
- **The folder is the queue.** Previous and next are the neighbours of the same kind (songs for
  a song, videos for a video, pictures for a picture) in the order the folder is shown in.

## Video

The video fills the viewer on black, whole picture visible. Over it lies one layer that appears
and disappears as a whole:

- top: back, the file's name, More (the menu the viewer already has);
- centre: back 10 seconds, play or pause, forward 10 seconds;
- bottom: the position bar with elapsed and remaining time, then previous, next, speed
  (0.5× to 2×), and full screen. Desktop and browser also get volume. Previous and next are
  absent when the folder has no other video.

Showing and hiding:

- While playing, the layer fades out 3 seconds after the last touch, key or pointer movement.
  On desktop the pointer hides with it.
- It stays while paused, while loading, at the end, on an error, while scrubbing, and while
  keyboard focus is inside it.
- One tap anywhere on the picture toggles it, at once. A second tap within 300 ms on the left or
  right third seeks 10 seconds instead and puts the layer back as it was.
- Pressing and holding for 2× leaves the layer's state alone: hidden stays hidden.
- Hidden controls cannot be focused or pressed.

The position bar: dragging shows the time under the finger and moves the picture only on
release, so a slow connection gets one request, not fifty. It shows how much is buffered. A
stall longer than 400 ms shows a spinner in the centre.

Reaching the end stops there and shows the layer. Videos do not start the next one by
themselves.

**Resume.** A video or recording of ten minutes or more opens where it was left, unless that was
in its last 5 percent. A chip "Resumed at 12:40 · Start over" shows for five seconds. Positions
are kept on the device for the last 200 files, keyed by machine, path, size and modified time.

### Full screen

One operation, `immersive(on)`, that each platform implements. The viewer calls nothing else.

| Where | Entering does | Notes |
| --- | --- | --- |
| Android app | hides status and navigation bars (a swipe from the edge shows them briefly), draws into the cutout, turns to landscape for a wide video and portrait for a tall one | works with the phone's rotation lock on; Android 16+ tablets keep the tablet's current orientation while still hiding the system bars |
| iOS and iPadOS app | hides the status bar and home indicator, turns the same way | on iPadOS with multitasking, as on large Android screens, the bars hide and the video fills while the device is not turned for you |
| Desktop app | makes the window full screen; leaving restores what it was | a window that was already full screen stays so |
| Browser | full screen on the viewer, not on the video, so Legion's controls stay; turns to landscape where the browser allows | on a browser without it (iPhone Safari), the system's video full screen; where neither exists, no button |

- The full-screen button toggles it. So does `F`; `Esc` and the back gesture leave it.
- Turning a phone or tablet to landscape while a video is open enters it; turning back leaves
  it, if turning is what entered it.
- Back, when full screen was entered with the button: leaves full screen. Otherwise: closes the
  viewer. Closing the viewer always ends it and gives the screen back as it was.
- The screen stays on while a video plays, in or out of full screen, and is released on pause,
  at the end and on close.
- In landscape nothing of the app remains around the video: no tab bar, no header.

Pictures use the same operation: a tap hides the viewer's bars, and with them the system's.

### Touch

One recogniser decides what a touch on the picture is, from the first pointer down on the picture
(never on a control):

| What happens | It is |
| --- | --- |
| a second pointer goes down | a pinch; anything else in progress is dropped |
| the finger moves more than 12 px, mostly sideways (within 30 degrees of horizontal), and the layer is visible | a scrub |
| the finger stays within 12 px for 350 ms, and the video is playing | a hold |
| the finger lifts before 350 ms within 12 px | a tap (one tap toggles the layer; a second within 300 ms on the left or right third seeks 10 s) |
| anything else (a vertical drag, a sideways drag with the layer hidden) | nothing, and it does not turn into a tap on release |

Touches that begin within 24 px of the left or right edge, or in the system's bottom gesture area,
belong to the system (back, home) and start nothing. The picture surface takes `touch-action: none`
while the viewer is open so the page never zooms or scrolls under these gestures. Once a touch has
become a pinch, scrub or hold, its release is never a tap. The mouse takes part where it makes
sense: press and hold is the same hold; a sideways drag with the button down is the same scrub.

**Zoom to fill.** Two states, nothing in between: *fit* (the whole picture) and *fill* (the
picture covers the screen, cropped evenly on two sides). While pinching, the picture follows the
fingers continuously between the two scales, with slight resistance beyond either end; on release
it settles on the nearer one, or the one the fingers were moving toward if moving fast, in 180 ms.
A chip at the top says "Zoomed to fill" or "Original" for 1.2 s. It exists only in full screen,
and only when fill would differ from fit by more than 3 percent; otherwise the pinch does nothing
and nothing is offered. In fill the picture runs under the display cutout while the controls keep
their safe margins. The choice is remembered on the device and applied when a video enters full
screen, but only if fill would hide at most 15 percent of the picture; a 4:3 film on a tall phone
starts whole. Desktop and browser: `Z` toggles it in full screen, and More carries "Zoom to fill"
or "Fit to screen" under the same conditions.

**Hold for 2×.** While held, the video plays at 2×; on release it returns to the speed that was
chosen before, not to 1×. A pill at the top centre shows "2×" with two small arrows for as long as
it lasts and fades in 120 ms. The layer's state does not change: hidden stays hidden. Absent when
paused or when the chosen speed is already 2×.

**Scrub anywhere.** With the layer visible, a sideways drag on the picture takes hold of the
position from where it is: the finger's travel moves the position by the same amount the bar would
(the bar's full width is the whole duration), starting at the current position rather than jumping
to the finger. While scrubbing, a large time is shown in the centre (such as "12:40") with the
change under it (such as "+0:35"), the bar's thumb follows, the layer stays visible, and the
centre transport buttons hide so the time is readable. As on the bar, the picture moves once, on
release. Pulling the finger down or up from where the drag began slows the scrub to half beyond
80 px ("Half speed"), a quarter beyond 160 px ("Quarter speed"), and a tenth beyond 240 px
("Fine"); moving back toward the line restores it. A scrub that ends within 12 px of where it
began changes nothing.

**Touch answers.** One platform operation next to `immersive`: `haptic(kind)` with `tick` (a state
changed: zoom settled, hold engaged, or a scrub rate changed) and `tap`. On Android it uses the
view's own haptic feedback constants, which need no permission and respect the system setting; on
iOS and iPadOS it uses the system's impact feedback. Everywhere else it is absent and callers do
not care. The browser vibration API is not used.

### Keys (desktop and browser)

Space or `K` play and pause (holding Space longer than 350 ms while playing plays at 2× until
release without toggling play or pause); left and right 10 seconds; up and down volume; `M` mute;
`F` full screen; `Z` toggles zoom to fill in full screen; `N` and `P` next and previous; `Esc`
leaves full screen, then closes.

## Music

A song opens on a screen made for listening:

- the cover, large and square; behind everything the same cover, blurred and dimmed. Without a
  cover: the music tile as today and a plain background;
- the title, then "artist · album". Without tags the title is the file name without its ending,
  and the second line is absent. Never "Unknown artist";
- the position bar with times; previous, play or pause, next; speed in More;
- in landscape and on wide windows: cover left, everything else right.

Controls never hide here. When a song ends the next one in the folder starts; after the last
one it stops. Previous is always shown on the music screen: on the first song or more than 3
seconds in it restarts the song, else it goes one back. Recordings of ten minutes or more resume
like videos do.

Where the platform has media keys or a lock-screen player (`navigator.mediaSession`), Legion
gives it title, artist, album, cover, and play, pause, previous, next and seek.

In the folder listing a song shows its cover where a picture shows its thumbnail.

### What the machine reports

- **`fs.tags`** (read): `{paths: [1..32]}` returns one entry per path, in order:
  `{path, ok: {title?, artist?, album?, album_artist?, track?, disc?, year?, duration_s?,
  cover}}` or `{path, error}`. `cover` is a cheap check that a picture was found (embedded or in
  the folder), not that it decodes; the caller shows its placeholder if `fs.thumb` then fails.
  Fields the file does not carry are absent. A file that is not audio is `unsupported`, per path.
- **`fs.thumb`** accepts audio files: the picture is the embedded cover (the front cover when
  several, else the first); when none exists or the embedded picture cannot be decoded, `fs.thumb`
  falls back to a picture in the same folder named `cover`, `folder`, `front` or `album` (jpg,
  jpeg, png, webp; any case), advancing to the next candidate if a candidate cannot be decoded.
  Only when no embedded or folder candidate decodes does it return `not_found` for that path.
  `size` may now be up to 1024, for the player.
- Read from: ID3v2 and ID3v1 (mp3, also inside wav and aiff), FLAC blocks, Vorbis comments in
  Ogg Vorbis and Opus (including `METADATA_BLOCK_PICTURE`), and MP4 items (m4a, m4b, mp4).
- These files are untrusted input. Tags and the cover are read inside the same out-of-process
  worker that decodes pictures, under the same memory cap and time limit, from the handle the
  node already opened. At most 16 MiB of a file's tag data is read; a declared length is never
  believed beyond that; a cover is then decoded like any picture, with every limit a picture
  has. Text is trimmed, stripped of control characters and cut to 512 characters a field.
- Allowed exactly when `fs.read` of the file is; a cover file in the folder, exactly when
  `fs.read` of that file is. Results share the thumbnail cache and its key.
- A machine that has neither tells the app so the usual way (per-machine capabilities); the app
  then shows the file name and the music tile, with no error.

## Keeping the music playing

Music belongs to the app, not to the screen that opened it. There is one player across the whole
app:

- Leaving a song's screen keeps it playing. Opening another song stops the first and revokes its
  stream before starting the new one. Starting a video pauses the music. Two songs never play at
  once.
- **The small player.** Whenever a song is loaded and the song's screen is not open, a 56 px bar
  sits directly above the tab bar on a phone (or at the bottom of the main column on desktop): a
  40 px square cover with 8 px corners (or the music tile), a single-line title over a single-line
  artist (never "Unknown artist"), play or pause, next when another song follows, and a 2 px
  position line along the top.
- Tapping the bar returns to the song's screen, growing the cover from the bar in 220 ms (a clean
  cut under reduced motion). Swiping the bar down dismisses it, stops playback and releases the
  stream. Ten continuous minutes paused dismisses it the same way.
- The bar is absent on the song's screen, in the video player, and over full-screen pictures.
  Scrollable views add its height as bottom padding while it is shown so the last row, the
  selection bar and the tab bar never sit under it.

### Shuffle and repeat

Two quiet toggles sit to the left and right of the transport on the song's screen, remembered on
the device:

- **Shuffle** shuffles the rest of the folder once, keeping the current song first, and holds that
  order for Previous, Next and track end until turned off. Turning it off returns to the folder's
  order around the current song.
- **Repeat** cycles through off, repeat folder, and repeat one. Repeat folder wraps Previous, Next
  and track end around the folder; repeat one replays the current song when it ends while explicit
  Next still moves on if another song exists.

### Android

WebView halts background track advances when the screen turns off and does not register a reliable
system `MediaSession` or media notification. On Android, Legion plays music in Media3 `ExoPlayer`
inside a `mediaPlayback` `MediaSessionService`, keeping one shared state with the page:

- **Fresh streams without the page awake.** The service talks to Rust over an owner-only (`0o600`,
  256-bit token) Unix domain socket inside the app's private data directory. On every song start,
  screen-off auto-advance, repeat, or lock-screen skip, Rust revokes the previous ticket, mints a
  fresh short-lived loopback content URL, and reads `fs.tags` and `fs.thumb` so the lock screen
  and notification stay current without waking the WebView.
- **Lock screen and notification.** Shows cover, title, artist, play or pause, previous, next (only
  when a next song exists), and a seekable position bar. Tapping the notification opens Legion on
  the song's screen, even if the activity was swiped away from Recents while music kept playing.
- **Notification permission.** Asked once, when music starts for the first time, with a single
  sentence explaining why. Refusing still plays.
- **Focus and headphones.** Unplugging headphones (`ACTION_AUDIO_BECOMING_NOISY`) pauses. Permanent
  audio-focus loss pauses and stays paused; a transient duck lowers volume and restores it; a
  transient loss such as a call pauses and resumes when the call ends if music was playing before.
- **Wake locks.** Held only while music is actively playing (and briefly around the track-switch
  ticket mint while advancing); released immediately on pause, stop, error, or dismiss.
- **Revocation.** While playing, Rust checks device access on the node's two-second cadence in
  addition to stream failures. If this device is revoked on the machine, playback stops promptly
  within the revocation watchdog's window, revokes the ticket, drops the foreground service, and
  shows a clear sentence naming the machine rather than playing buffered silence or looping.

### iOS and iPadOS

The same small player and single music state run on iOS and iPadOS. The app declares the `audio`
background mode and attaches an origin-scoped (`tauri://localhost`, main frame only) Swift bridge
that configures `AVAudioSession` with `.playback`, publishes title, artist, album, cover, duration
and position to `MPNowPlayingInfoCenter`, wires `MPRemoteCommandCenter` (play, pause, toggle,
previous, next when present, and scrubbing), pauses when headphones unplug
(`.oldDeviceUnavailable`), and pauses and resumes across call interruptions.

### Desktop and browser

Desktop and browser share the same small player and single music state. `navigator.mediaSession`
stays active while the small player is shown and clears when dismissed. Hiding or closing the
desktop window (for example while transfers drain or for an update) pauses music in place so
nothing keeps playing behind a hidden window.

### Later

Picture-in-picture for video remains for a later step.

## Tests

- Interface: the layer's timing with a clock under test control (hides after 3 s while playing;
  stays while paused, loading, ended, on error, while scrubbing, with focus inside); tap toggles;
  double tap seeks and restores; one seek per drag; the touch recogniser (pinch, scrub, hold, tap,
  edges, thresholds, second finger during scrub); zoom to fill (nearer state, velocity, 3 percent
  and 15 percent rules, `Z` and More); hold for 2× (pointer and Space, restores chosen speed);
  scrub anywhere (bar-matched travel, slower rates, 12 px cancel); 10-second seek icons centred at
  device pixel ratios 1, 2 and 3; no disabled control anywhere; queue order follows the folder's
  sort; song ends, next starts; resume and "Start over"; keys.
- Small player, single state, shuffle and repeat: leaving the song's screen keeps playing and shows
  the 56 px bar directly above the tab bar on phones and at the bottom of the main column on
  desktop with zero overlap against rows, the selection bar or tabs; absent on the song's screen,
  video player and full-screen pictures; tapping returns to the song's screen; swiping down and ten
  continuous paused minutes (under a controlled clock) dismiss and revoke the stream; opening a
  second song revokes the first; starting a video pauses music; shuffle keeps the current song
  first and preserves order until turned off; repeat cycles off, folder and one and persists across
  reloads.
- Android background playback: on the Android emulator, play a folder of six real one-minute tagged
  songs with embedded covers, lock the screen (`KEYCODE_POWER`) and let it play for five full
  minutes; verify the native 1-second position log has no gap larger than 2 seconds against wall
  time, at least four songs play through in order, `MediaSession` metadata and the notification
  match each song, `cmd media_session dispatch next` advances, zero wake locks are held while
  paused, revoking the device on the node stops playback within the revocation watchdog's window
  with a clear sentence, and focus/headphone events pause and resume as specified.
- `immersive`: on the Android emulator, screenshots prove the bars are gone and nothing of the
  app surrounds the video, in both orientations, by button and by turning, and that back and
  close restore them. Desktop: the window's state before and after. Browser tests: the viewer
  element is the full-screen element.
- `fs.tags` and covers: fixture files for every format above, with and without tags and
  pictures, several pictures, a folder cover, wrong and oversized declared lengths, truncated
  files, hostile text; a fuzz target for the parser; the worker's memory cap holds.
- On a real-size library (a folder of a few hundred songs): the listing stays responsive while
  covers load, and opening a song shows its title before its cover.
