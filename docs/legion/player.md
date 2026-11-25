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
- It stays while paused, while loading, at the end, on an error, and while keyboard focus is
  inside it.
- One tap anywhere on the picture toggles it, at once. A second tap within 300 ms on the left or
  right third seeks 10 seconds instead and puts the layer back as it was.
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
| Android app | hides status and navigation bars (a swipe from the edge shows them briefly), draws into the cutout, turns to landscape for a wide video and portrait for a tall one | works with the phone's rotation lock on |
| iOS and iPadOS app | hides the status bar and home indicator, turns the same way | |
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

### Keys (desktop and browser)

Space or `K` play and pause; left and right 10 seconds; up and down volume; `M` mute; `F` full
screen; `N` and `P` next and previous; `Esc` leaves full screen, then closes.

## Music

A song opens on a screen made for listening:

- the cover, large and square; behind everything the same cover, blurred and dimmed. Without a
  cover: the music tile as today and a plain background;
- the title, then "artist · album". Without tags the title is the file name without its ending,
  and the second line is absent. Never "Unknown artist";
- the position bar with times; previous, play or pause, next; speed in More;
- in landscape and on wide windows: cover left, everything else right.

Controls never hide here. When a song ends the next one in the folder starts; after the last
one it stops. Previous restarts the song when more than 3 seconds in, else goes one back.
Recordings of ten minutes or more resume like videos do.

Where the platform has media keys or a lock-screen player (`navigator.mediaSession`), Legion
gives it title, artist, album, cover, and play, pause, previous, next and seek.

In the folder listing a song shows its cover where a picture shows its thumbnail.

### What the machine reports

- **`fs.tags`** (read): `{paths: [1..32]}` returns one entry per path, in order:
  `{path, ok: {title?, artist?, album?, album_artist?, track?, disc?, year?, duration_s?,
  cover}}` or `{path, error}`. `cover` says whether `fs.thumb` has a picture for it. Fields the
  file does not carry are absent. A file that is not audio is `unsupported`, per path.
- **`fs.thumb`** accepts audio files: the picture is the embedded cover (the front cover when
  several, else the first), else a picture in the same folder named `cover`, `folder`, `front`
  or `album` (jpg, jpeg, png, webp; any case). None: `not_found` for that path. `size` may now
  be up to 1024, for the player.
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

## After that: it keeps playing

Built once the above is in use, as its own piece of work:

- a small player bar above the tabs while browsing, so leaving the song's screen does not stop
  the music;
- on Android, playing on with the screen off or the app in the background, with the system's
  media notification and lock-screen controls. The test that decides the mechanism: play a
  folder of real songs, lock the screen, ten minutes later it is still playing and has moved to
  the next track;
- shuffle and repeat; picture-in-picture for video.

## Tests

- Interface: the layer's timing with a clock under test control (hides after 3 s while playing;
  stays while paused, loading, ended, on error, with focus inside); tap toggles; double tap
  seeks and restores; one seek per drag; no disabled control anywhere; queue order follows the
  folder's sort; song ends, next starts; resume and "Start over"; keys.
- `immersive`: on the Android emulator, screenshots prove the bars are gone and nothing of the
  app surrounds the video, in both orientations, by button and by turning, and that back and
  close restore them. Desktop: the window's state before and after. Browser tests: the viewer
  element is the full-screen element.
- `fs.tags` and covers: fixture files for every format above, with and without tags and
  pictures, several pictures, a folder cover, wrong and oversized declared lengths, truncated
  files, hostile text; a fuzz target for the parser; the worker's memory cap holds.
- On a real-size library (a folder of a few hundred songs): the listing stays responsive while
  covers load, and opening a song shows its title before its cover.
