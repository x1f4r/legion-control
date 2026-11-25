# Files: browsing, moving, finding and keeping folders in sync

This is the design for Legion's file experience: what a person sees, what an agent can call, and
how bytes move. It builds on [design.md](design.md) (tools, the bulk-data pipe, permissions) and
[platforms.md](platforms.md) (nodes, controllers, the three pipes). Where this document and the
code disagree, the code is wrong or this document is out of date; fix one of them.

## What it has to do

1. Browse any machine's files from any device, with a back gesture that steps back one folder.
2. Open a file in place: pictures as pictures, video and audio played straight from the machine
   with seeking, text as text, everything else handed to the device ("download", "open in another
   app").
3. Copy or move files between two machines when neither is the device in hand, and keep going
   when the device goes away.
4. Find files by name and kind on one machine or all of them. Star files and folders, pin the
   ones used most.
5. Keep a folder in sync between chosen machines without any machine being a hub.
6. Everything a person can do here, an agent can do with a tool.

## Principles

- **No new trust.** Every byte travels over a connection Legion already has (the in-app ssh
  session, the local child, the HTTP door, node-to-node sessions) and is checked against the same
  `policy.toml` rule as the `fs.*` tool it amounts to. Nothing listens on a network port that did
  not listen before.
- **JSON carries control, never bulk.** Thumbnails fit in a reply. File content moves on the bulk
  pipe (`legion stream`) and reaches a viewer through a content address (below).
- **A node only writes to its own disk.** Transfers a person starts are pushed by the node that
  holds the file, as today. Sync is pull-only: a node fetches what it lacks from a peer that
  agrees the folder is shared with it.
- **Nothing is lost quietly.** Moving removes the source only after the destination's hash
  matches. Sync never deletes bytes it did not write itself: anything moved out of a person's tree
  is identified by hash and either moved to the folder's bin (when it matches the indexed version)
  or kept as a conflict copy beside its path (when it is an unseen local save). Only staged
  downloads proven by a write-ahead record are unlinked from `.legion-sync/tmp`. Two versions of
  one file are both kept.
- **Plain words.** Folder, picture, video, music, document, bin, "kept in sync". Paths, hashes
  and tool names appear only with Technical details on.

## Kinds of file

One table in `legion-core` (`media`) maps a file extension to a **category** and a media type:

| category | examples |
| --- | --- |
| `image` | jpg, jpeg, png, gif, webp, avif, heic, bmp, svg, tif |
| `video` | mp4, m4v, mov, webm, mkv, avi |
| `audio` | mp3, m4a, aac, flac, ogg, opus, wav |
| `document` | pdf, epub, doc(x), xls(x), ppt(x), odt, ods, odp |
| `text` | txt, md, log, csv, json, toml, yaml, xml, html, css, source code |
| `archive` | zip, tar, gz, xz, zst, 7z, rar |
| `other` | everything else |

The table is by extension only, case-insensitive; `fs.list` and `fs.stat` never open files to guess.
As a targeted exception for `.ts`, `fs.find` and `fs.thumb` sniff the leading bytes of regular `.ts`
files for MPEG-TS sync packets (`0x47` at 188/192-byte intervals, rejecting plain UTF-8 text) so
video recordings named `.ts` match `categories: ["video"]` (`video/mp2t`) and thumbnail with
`ffmpeg`, while TypeScript `.ts` files remain `text` (`text/typescript`). `fs.list`, `fs.find` and
`fs.stat` report `category` and `mime` for regular files. The existing `kind` (`file`, `dir`,
`symlink`, `other`) keeps its meaning.

## Tools

Additions are optional fields and new tools, so an older client keeps working against a newer
node.

### Looking

- `fs.list` gains `sort` (`name`, `modified`, `size`), `order` (`asc`, `desc`), `categories`
  (list, keeps directories), `offset`, and reports `total`, `total_exact` and `truncated`.
  Directories still come first; `name` uses natural case-insensitive order (`file2` before
  `file10`). `depth: 1` always counts every direct child (`total_exact: true`); recursive
  listings (`depth > 1`) stay on one filesystem and bound counting once the requested page is
  full, reporting a lower-bound `total` with `total_exact: false` when cut short.
- `fs.find` gains `name` (whitespace-separated words that must all appear in the file name,
  case-insensitive, Unicode-aware), `categories`, `min_size`, `max_size`, `modified_after`,
  `modified_before` (RFC 3339 timestamps in the same format `fs.stat` and `fs.list` return),
  `sort`, `order`, `hidden` (separate from `ignore`, defaulting to `false` when `ignore` is
  `true` and `true` when `ignore` is `false`, so a search can skip hidden entries without obeying
  `.gitignore`) and `budget_ms` (default 10 000, at most 60 000). Without `sort`, results stream
  in directory-entry order for speed; pass `sort` when deterministic ordering is needed. Each
  reply carries `machine` (the node that produced it); a search that runs out of time or reaches
  `limit` returns what it has with `truncated: true` and `reason` (`time` or `limit`). With
  `on: "all"` it fans out concurrently across the fleet, returning `{"results": {"<machine>":
  {"ok": {machine, matches, truncated, reason?}} | {"error": {code, message, hint?, machine}}}}`
  so unreachable machines are reported per key rather than failing the search.
- `fs.thumb` (read): `{paths: [1..32], size: 96..512 = 256, fit: "contain" | "cover"}` returns
  exactly one entry per input path in input order: `{"results": [{path, ok: {mime, content
  (base64), width, height}} | {path, error}]}`. Regular files are opened non-blockingly once
  (`O_NONBLOCK | O_NOCTTY` plus `fstat` regular-file verification on Unix; disk file check on
  Windows) across `fs.*` and `legion-xfer` so swapping a regular file for a FIFO or special file
  cannot block a node thread. Pictures are header-peeked (`<= 128 KiB`, cross-checking inner frame
  dimensions in JPEG/GIF/WebP against outer container headers and walking GIF extension blocks up
  to the peek bound without a fixed block cutoff), reserved against the shared in-flight byte
  budget before reading the full file, and decoded out-of-process in a memory-bounded worker that
  receives the already-opened file handle on `stdin`, starts with a cleared minimal environment,
  validates its CLI arguments strictly before allocating, and opens nothing by path (JPEG, PNG, GIF
  first frame, WebP, BMP; EXIF orientation applied). Admission caps source files at 64 MiB and
  estimated peak decode memory at 256 MiB per image (admitting 48–61 MP baseline JPEGs while
  capping heavier progressive/16-bit/interlaced encodings). Over-cap JPEGs fall back to a validated
  embedded MPF preview (preferred) or EXIF IFD1 thumbnail through the same limits with the main
  image's EXIF orientation, returning a per-path `too_large` error only when none is usable. Every
  worker enforces an exact cross-OS live-heap cap via a capped global allocator (sized at
  `1.35x estimated_peak + 8 MiB`, floored at `16 MiB`, exiting immediately on overshoot; unlimited
  in the normal node at the cost of one relaxed atomic load) backed by a 10 ms RSS watchdog on
  every platform (including macOS) set above the measured per-platform worker startup baseline plus
  `heap_cap` plus `max(6 MiB, 10% of heap_cap)` headroom, an OS backstop limit above that watchdog
  limit (`RLIMIT_AS` with virtual-address headroom on Linux, `RLIMIT_DATA` on other non-macOS Unix,
  suspended-create Job Object memory limit on Windows), and a 10 s wall-clock kill timeout. Each
  worker's in-flight reservation equals its watchdog resident limit (`baseline + heap_cap + max(6
  MiB, 10%)`), so the sum of live image and `<= 1080p` video worker limits never exceeds the shared
  budget of `min(512 MiB, effective RAM / 8)` (floored at 256 MiB, honoring Linux cgroup v2/v1 and
  Windows Job Object memory limits), scheduled round-robin by principal with anti-starvation aging
  across up to 16 concurrent decodes (scaled to CPU cores, bounded by the shared byte budget) and a
  10 s bounded wait (returning retryable `busy`). Only allocator-cap memory kills (when the capped
  allocator itself stops an image exceeding its heap cap) are recorded as negative bad-file entries
  in `thumb-cache/` keyed to file identity plus `size` and `fit`, and those entries expire after
  1 day so the file is retried after expiry; backstop kills (`rss_watchdog` / OS limit while the
  allocator stayed under cap), external signals (`SIGKILL`, `SIGTERM`), and crashes return a
  transient per-path error without poisoning the cache (logging baseline/cap/limit/peak
  diagnostics). Timeouts are never remembered permanently as bad; repeated CPU-busy timeouts of the
  same file/size/fit trigger a short 5-minute in-memory back-off. Output is JPEG, or PNG when the
  source has transparency. Video posters (including sniffed MPEG-TS `.ts` recordings) run `ffmpeg`
  by resolved absolute path with a cleared minimal environment and `MALLOC_ARENA_MAX=2` (capping
  glibc arena reservations; `-threads 1` limits decoder, encoder, and filter threads rather than
  making `ffmpeg` strictly single-threaded), under the RSS watchdog as the deciding resident limit
  plus a +1 GiB virtual-address backstop (256 MiB resident cap for `<= 1080p` videos sharing the
  512 MiB pool; `> 1080p` 4K/8K videos drain the entire shared pool and all video slots so they run
  strictly one at a time with zero concurrent images or videos under an exclusive `min(768 MiB,
  max(256 MiB, effective RAM / 4))` resident budget, preserving rotation metadata; no shell,
  `-max_pixels 67108864`, `-max_alloc 134217728`, `-protocol_whitelist file`, MOV/MP4 demuxer
  pinned with `-enable_drefs 0 -use_absolute_path 0`, non-MOV `-format_whitelist` excluding MOV and
  playlists/manifests, video-only mapping, 15 s time limit) when `ffmpeg` is on `PATH` and report
  `unsupported`
  otherwise. Positive thumbnails and expiring bad-file entries share a process-wide lock-free cache
  handle in the state directory (`thumb-cache/`) keyed by path, OS file identity, metadata change
  time, source size, modification time and parameters, reconciled directly on disk against the
  256 MiB LRU cap. A thumbnail of a file is allowed exactly when `fs.read` of it is.

### Moving

`fs.transfer` gains:

- `if_exists`: `fail`, `skip`, `replace` or `rename` (default `replace`, as today; `rename`
  writes `name (2).ext`).
- `move`: after the destination is verified by hash, remove the source. It needs the caller to be
  allowed `fs.remove` on the source and is refused before any byte moves otherwise. A tree is
  verified file by file and a source file is removed only once its copy is verified; an
  interrupted move leaves every file in at least one place.
- `background`: start the transfer as a job on the node that runs it and return
  `{job: {id, machine}}` at once. The job belongs to the caller's principal, is listed by
  `job.list` with `kind: "transfer"` and a `title` a person can read ("Copy Holiday.mp4 to pi"),
  reports `progress` (`bytes_done`, `bytes_total`, `files_done`, `files_total`, `current`,
  `rate`) in `job.status`, and survives the caller disconnecting. Its owner may cancel it with
  `job.cancel` without holding `effect:exec`; cancelling keeps the partial file, so starting the
  same transfer again resumes.

A controller starts a machine-to-machine transfer on the source machine
(`on: A`, `from: /path`, `to: "B:/path"`), and on the destination when the source cannot reach
it.

### Favourites and pins

A favourite is a file or folder on a machine that someone starred. A pinned favourite is also
shown at the top of Files. They belong to the fleet, not to a device, so the phone and an agent
see the same list.

- `fav.list` (read): `{machine?, pinned?}` returns entries
  `{id, machine, path, kind, category?, label?, pinned, added_at, added_by}` and, cheaply,
  whether each still exists where its machine is this node.
- `fav.add` (write): `{machine?, path, label?, pinned?}`. `fav.set`: `{id, label?, pinned?}`.
  `fav.remove`: `{id}`.

They live in `favorites.toml` beside `policy.toml`, a shared document like `devices.toml` but
with no bearing on access: each entry carries a stamp (`updated_at`, `updated_on`), the newer
stamp wins field by field, a removal is a stamped tombstone kept for 90 days, and two nodes that
exchange the file end up equal whatever the order. A node that changes it pushes it to the peers
it can reach, and `fleet.sync` and the sync runner carry it to the rest. `fav.list` leaves out
entries outside the caller's `paths`. One list per fleet is deliberate for now; a list per person
waits for people to exist in the model.

### Sync folders

See [Keeping folders in sync](#keeping-folders-in-sync): `sync.list`, `sync.create`, `sync.add`,
`sync.remove`, `sync.delete`, `sync.status`, `sync.now`, `sync.pause`, `sync.resume`,
`sync.conflicts`, `sync.resolve`, `sync.bin`, `sync.restore`.

## Reaching the bytes

A viewer needs a file's bytes with random access: a `<video>` element asks for ranges, a download
resumes from an offset.

### The pipe

`legion stream`'s `get` gains `length` and optional `keep_open: true`. Its reply line becomes
`{size, offset, length, mtime, file_id?, keep_open?}` followed by exactly `length` bytes (to the
end when absent), and a `{"done": true}` line when `keep_open` is active so one remote `legion
stream` pipe can serve many consecutive range requests without re-handshaking. An idle remote
`legion stream` process exits after 120 seconds of inactivity.

`legion-link` gains `Link::open_content(machine, path, offset, length)`, which opens that operation
on the session the link already has for that machine (a new exec channel on the ssh connection, or
a child for a local node) and returns the size, modification time, file identity, and an async
reader. On SSH connections:

- **Slots and fair waiting**: at most 8 active content channels run on one SSH connection (strictly
  below OpenSSH's default `MaxSessions` of 10, leaving room for RPC). When all 8 slots are actively
  reading, further opens wait fairly in FIFO order up to the SSH connect timeout before returning a
  retryable `busy` error; on teardown or parking, `legion-link` sends stdin EOF and holds the permit
  until `sshd` confirms channel close (`SSH_MSG_CHANNEL_CLOSE`), falling back to a local channel
  close after 2 seconds if a remote ignores stdin EOF.
- **Parking and transparent reopening**: a stream holds an SSH channel and slot only while actively
  read. After ~2 seconds of reader inactivity, `ContentStream` parks its channel — closing the SSH
  channel and freeing its slot while keeping already-buffered bytes (within a bounded per-channel
  window cap). On the next read after parking, or after any idle close (remote 120-second idle
  timeout or SSH session reconnect), it transparently reopens at the exact next byte offset,
  verifying `(size, mtime, file_id)` (failing cleanly if the file changed on disk) with bounded
  consecutive retries.
- **Misbehaving-remote bounds**: active remote reads enforce a 30-second per-window progress
  timeout (armed only while waiting on a live channel, never while parked) and a cap of 32
  consecutive `{"busy": true}` lines.

It follows the routing rules calls follow: it names its machine and never goes somewhere else.

### Content addresses

An `<img>` or `<video>` cannot send an `Authorization` header and must not run inside the
interface's origin. So content is served from a **content address**: a URL holding a ticket.

- A ticket is 256 random bits, made for one principal, one machine, and one bounded path (at most
  4 KiB; `POST /v1/content` bodies are capped at 16 KiB). It is checked as `fs.read` when made, on
  every request, and while a response streams: an independent 1-second liveness watcher per active
  stream (not tied to client read speed or body polling) re-checks the door's own bearer token,
  local `policy.toml` (principals and paths), and the 6-hour hard cap, and whenever a write resumes
  after socket backpressure the body re-checks liveness before yielding another 64 KiB frame.
  Revoking a token or removing a principal revokes every ticket of that holder; a per-ticket failure
  (a narrowed path, expiry, or crossing the 6-hour cap) ends only that ticket and leaves the
  holder's other still-allowed tickets serving. Each active content socket requests `SO_SNDBUF =
  128 KiB` before sending the response (the effective kernel buffer varies by OS: Linux doubles it
  to 256 KiB for bookkeeping; macOS sets ~128–147 KiB; Windows sets 128 KiB), and as soon as
  revocation is detected the server cancels the connection first (before flushing audit records to
  disk), hands no further bytes to the socket, discards its own buffered bytes, releases its remote
  pipe, and resets the TCP connection (`SO_LINGER(0)`) to drop the server's unsent kernel send
  queue. What this guarantees for what the door itself decides (its bearer tokens, local principals
  and paths, and ticket lifetime):
  1. **Time**: server-side cutoff within 2 seconds of revocation even for slow (e.g. 20 KB/s) or
     completely stalled readers.
  2. **Zero bytes after cutoff**: once cutoff is reached, zero further bytes are handed to the
     socket and the connection closes (EOF or reset).
  3. **Physical byte ceiling between revocation and cutoff**: at most the socket's effective
     `SO_SNDBUF` plus the client's effective `SO_RCVBUF` plus whatever bytes the client actually
     reads during that window plus one 64 KiB write chunk (collapsing to `SO_SNDBUF + SO_RCVBUF +
     64 KiB` for a stalled reader).
  What lies outside those bounds:
  - **Remote machine policy changes**: the door does not watch another machine's `policy.toml`. For
    a file served from a remote machine, tightening policy on that machine takes effect at that
    machine's next check of the stream (the next window open/reopen), which a slow reader delays,
    with the slow-reader timeout as the backstop.
  - **Proxies and client receive buffers**: the `SO_SNDBUF` bound (256 KiB effective on Linux)
    applies to the loopback hop only; behind Tailscale Serve (or another reverse proxy), its own
    buffers sit beyond it. Bytes already transmitted and accepted into the peer's own TCP receive
    buffer (or in flight on the network) before revocation cannot be recalled, so a reader may still
    drain whatever its OS had already buffered (which varies by OS and receive-buffer autotuning,
    e.g. ~128–256 KiB on Linux loopback and ~384–650 KiB on macOS loopback).
  Expiry uses a suspend-safe clock (the maximum of wall-clock and monotonic elapsed time): 15
  minutes after its last use and 6 hours at most, capped per holder (128, LRU eviction) and
  globally.
- The address answers `GET` and `HEAD` with single-range support (`Range`, `If-Range`, a strong
  `ETag` from size, modification time, and Unix file identity, `206` and `416` done properly),
  streaming with backpressure and never reading a file into memory.
- Every reply carries `X-Content-Type-Options: nosniff`, `Cross-Origin-Resource-Policy`
  (`same-origin` on the HTTP door; `cross-origin` on a native loopback mount), `Referrer-Policy:
  no-referrer`, `Cache-Control: private, no-store`, and unconditionally
  `Content-Security-Policy: sandbox; default-src 'none'; frame-ancestors 'none'`. `Content-Type`
  comes from a short allow-list of types safe to show in place (raster images, SVG, video, audio,
  `text/vtt; charset=utf-8` for `.vtt` subtitles, and `text/plain; charset=utf-8` for every other
  text file, including HTML). Everything not on the list — including PDF — is served as
  `application/octet-stream` with `Content-Disposition: attachment`. The test that matters: an
  HTML, SVG, or PDF file opened through a content address never runs script or embeds with access
  to the interface.
- `?download=1` forces `Content-Disposition: attachment` with the file's name.
- **Concurrent reads and slow readers**: active content streams are capped per ticket (8), per
  holder (16), and globally (64), refusing excess streams with `429 Too Many Requests`,
  `Retry-After`, and `Connection: close`, and stalled or overly slow readers are closed after an
  idle write timeout (15 s) or minimum progress window (40 KiB / 10 s) so one holder cannot starve
  the API or UI.
- **Audit durability and coalescing**: minting a ticket writes one durable `content.ticket` entry
  (never logging the ticket id); the first `GET` or `HEAD` on a ticket immediately writes one
  durable `content.open` entry (`who`, `machine`, `path`) before streaming bytes so every access
  survives a hard process kill (`SIGKILL`) or power loss; ongoing reads coalesce to at most one
  `content.read` summary per ticket per minute (and one `stream.get` entry per principal/path per
  minute on a remote node) with request count and bytes served, flushed on expiry, revocation, or
  clean shutdown.
- **Lifetime and browser history**: a content address stays usable for up to 6 hours while in active
  use and will appear in browser history if opened directly in a tab. Damage is bounded because the
  ticket is read-only to one file, expires after 15 minutes of idleness (and 6 hours unconditionally),
  cuts off active readers within 2 seconds when the door revokes the bearer token, removes the
  principal, or tightens local `fs.read` policy (changes to a remote machine's `policy.toml` take
  effect at that machine's next check of the stream, backed by the slow-reader timeout), carries
  `Cache-Control: private, no-store` and `Referrer-Policy: no-referrer`, and renders only in a
  sandboxed opaque origin with `frame-ancestors 'none'`. If a third-party page somehow obtains a
  ticket, `Origin` checks block cross-origin `fetch()` unless explicitly listed,
  `Cross-Origin-Resource-Policy: same-origin` blocks no-cors subresource embedding,
  `frame-ancestors 'none'` blocks framing, and `sandbox; default-src 'none'` isolates any top-level
  navigation.

One module (crate `legion-content`) holds the ticket table, the range handling, concurrency limits,
the `Host`/`Origin` guard, and the headers, over a small trait that opens a byte range. Two things
mount it:

- **The HTTP door**: `GET /v1/health` advertises `"content": true` so the web UI enables content
  addresses once on connect and remembers per-machine `unsupported` replies for the session when
  browsing an older remote node. `POST /v1/content` (bearer token) with `{on, path}` returns
  `{url, expires_in}`; the URL is `/v1/content/<ticket>/<file name>` on the same origin, with
  `Cross-Origin-Resource-Policy: same-origin` always set (origins listed in `[serve] origins` read
  content via CORS `fetch()` or `crossorigin`, not no-cors embedding). The door reads remote files
  in bounded windows over a dedicated pool of warm `keep_open` `legion stream` pipes per target
  machine, separate from transfer and RPC pipes (which never share permits with content), with
  per-holder fair sharing and a short acquire timeout (`503 Service Unavailable` with `Retry-After`).
- **The app**: the native side mounts `legion-content` behind `content_url({machine, path,
  download})`, reading through `Link::open_content`. Linux, macOS, Windows, iOS and iPadOS mount it
  lazily on a listener bound to `127.0.0.1` on a system-chosen port (capped at 32 simultaneous
  connections with a 5-second header-read timeout, exponential backoff on accept errors, iOS
  resume recovery, and stopping 60 seconds after no live tickets or active streams remain),
  protected by `HostOriginGuard` (loopback host+port and only that platform's bundled page origins,
  never `null`) and `Cross-Origin-Resource-Policy: cross-origin`. Android keeps
  `android:usesCleartextTraffic="false"` in its manifest, which causes Chromium WebView to refuse
  cleartext loopback HTTP; Android therefore mounts the same `legion-content` crate on a Tauri
  custom URI scheme (`legion-content`, reached as
  `http://legion-content.localhost/v1/content/<ticket>/<name>` and intercepted in-process via
  `shouldInterceptRequest`), answering open-ended `Range` requests in bounded 2 MiB pieces,
  refusing unranged responses over 16 MiB with `413 Payload Too Large` before reading a byte, and
  capping total concurrent buffered memory at 24 MiB. Tickets are read-only, short-lived, exist
  only for files opened in the app, and die immediately when their pairing is removed or replaced
  or when the app quits.

The interface's content security policy grows by exactly what this needs: `img-src` and
`media-src` for `http://127.0.0.1:*` (desktop and iOS) or `http://legion-content.localhost`
(Android).

### To and from the device

- **Download to this device** (app): a native command streams the file into a private `.part` file
  with bounded 64 KiB memory, resumes from the partial file's byte offset across network drops,
  pauses, or restarts, verifies the completed file against `fs.hash` (BLAKE3), sanitizes the file
  name against the union of Windows, POSIX and Unicode control/format rules (stripping separators,
  drive prefixes, NTFS streams, BiDi overrides and reserved Windows device names), and places it
  without overwriting existing files (atomic `hard_link` / exclusive create with `name (2).ext`
  numbering) into the device's Downloads destination (Android: shared `Downloads/Legion` through
  `MediaStore.Downloads` with no broad storage permission; desktop: the user's Downloads folder,
  marked with `:Zone.Identifier` on Windows and `com.apple.quarantine` on macOS; iOS and iPadOS:
  `Documents/Downloads` exposed to Apple Files). In a browser it is a link to the content address
  with `?download=1`.
- **Open in another app** (app): downloads to the app's cache (`open-externally/`, pruned by count,
  age and total size), verifies the hash, derives MIME on the device strictly from the saved
  file's real extension (never trusting remote MIME claims), and hands safe types to the system
  viewer or chooser (always `Intent.createChooser` on Android via a non-exported `FileProvider`
  scoped solely to `cache/open-externally/`). Executables, installers, scripts, shortcuts, disk
  images and macro documents are never opened directly by Legion; finished rows offer "Show in
  folder" instead, and tapping their completion notification on Android opens Downloads.
- **Add from this device** (app): picks files exclusively via the native system file picker
  (`ACTION_OPEN_DOCUMENT` with persistable read URI grants released on completion/failure/cancel on
  Android; native file dialog on desktop) and issues session-only opaque upload handles so the web
  view can never name an arbitrary device file. Pre-selected local upload paths exist only behind
  the debug compile-time flag `test-upload-paths`. Files are hashed in 64 KiB chunks, checked with
  `fs.stat` to choose a non-colliding `name (2).ext` remote target before starting, and uploaded
  over the bulk pipe's `put` through `Link::open_put` with remote `.legion-part` resume and BLAKE3
  verification.

Nothing here makes a person wait. Starting a download, an upload or a transfer returns at once;
the interface stays free to browse, play something else or leave Files, and the work shows in
Activity. A viewer can be closed while it is still loading, which stops the loading.

Transfers to and from the device run in a queue on the app's native side, not in the web view,
so they do not depend on a screen being open:

- **Android**: a `dataSync` foreground service (`TransferForegroundService`) with one ongoing
  notification (what is moving, progress, Pause/Resume and Cancel; `POST_NOTIFICATIONS` requested
  lazily on first user transfer with a one-sentence rationale). It keeps going when the person
  switches apps, locks the screen or swipes Legion away from Recents. While all transfers are
  paused, the service detaches from foreground state so paused jobs do not consume Android 15+'s
  6-hour rolling `dataSync` quota while keeping Resume and Cancel buttons live. If Android's
  `dataSync` time limit fires (`onTimeout`), active transfers pause cleanly with a tap-to-resume
  notification and the service stops within Android's grace window. The queue is persisted to disk;
  after process death (`force-stop`, OOM) or reboot (Android 15+ forbids `BOOT_COMPLETED` receivers
  from starting `dataSync` foreground services), unfinished transfers resume from their partial
  files the next time Legion opens, and finished downloads post a notification that opens the file.
- **Desktop (Linux, macOS, Windows)**: the same queue. Closing the window while it is working
  hides the window, sends a system notification, and exits automatically once the queue drains;
  quitting outright asks for confirmation first. Unfinished transfers resume on the next start.
- **iPhone and iPad**: runs the same queue while Legion is in the foreground; when iOS suspends
  the app, active transfers pause automatically and resume when Legion returns to the foreground.
  `state()` reports `transfer_capabilities: { background: false, pauses_in_background: true,
  can_upload: false, can_open_externally: false, can_open_downloads: false }` and the app link
  omits upload and open-in-another-app until native iOS document picker / share-sheet bridges exist.
  Downloads land in `Documents/Downloads` visible in Apple Files (`UIFileSharingEnabled` and
  `LSSupportsOpeningDocumentsInPlace`).

Two transfers run at once by default while the rest wait in order, stalled streams pause
automatically with a visible reason, and a persisted Wi-Fi-only setting (default off; threshold
50 MB in SI decimal units matching the interface) pauses large transfers on metered networks and
resumes them automatically on unmetered Wi-Fi.

## The interface

The reference screens are `apps/ui/design/files.html`. The rules of the second pass still hold:
nothing without a word, daily actions in reach of the thumb, plain words first, off is just off.

- **A fourth tab.** Machines · Files · Activity · Actions. A machine's own Files part opens the
  same browser at that machine.
- **Files** (the tab's first screen): Pinned (tiles), Kept in sync, Machines (each machine's home
  folder, with the off ones quiet), Favourites, Recent (kept on this device only). The search box
  is docked at the bottom.
- **A folder**: a row per entry with a thumbnail or kind icon, name, and one line (kind, size,
  when). List or grid; the grid is for pictures and video. Sort, view and "more" sit in the dock,
  not at the top. Swiping a row shows Star and Send to; a long press starts selecting, and the
  dock becomes the action bar (Copy to, Move to, Download, Delete).
- **Every step is history.** The address holds the machine and the path
  (`#/files/<machine>/<path>`), and going into a folder, opening a viewer and opening a sheet
  each push one entry. So the system's back gesture, the browser's back button and the header's
  back arrow all do the same thing: close the sheet, then the viewer, then go to the folder this
  one was opened from, and leave Files only from its first screen. Sideways swiping between tabs
  is off inside a folder so it cannot fight the back gesture.
- **The viewer** covers the screen. Pictures zoom with a pinch or double tap, and swiping
  sideways goes to the next picture in the folder as it is currently sorted and filtered. Video
  and audio use the platform's own player with the content address. Text shows the first
  128 KiB and offers more. Everything else shows what it is and two buttons: Open in another app,
  Download. The viewer's actions (Download, Send to, Star, More) are at the bottom.
- **When a video will not play** the viewer says so in a sentence and offers Download and Open in
  another app. A phone plays what its own decoders play; Legion does not convert video.
- **Send to** is one sheet: Copy or Move, which machine (off machines are listed and say so, with
  Wake where it exists), which folder there (starting from pinned folders and the last places
  used), then one button that says what will happen ("Copy to pi"). The transfer runs in the
  background on the machines and appears in Activity with progress.
- **Search**: the docked box searches the current folder downwards, or every machine from the
  first screen. Kind chips (Pictures, Videos, Music, Documents, Folders) and "More filters"
  (size, changed) sit above the box. Results arrive machine by machine; a machine that is off is
  named as not searched.
- **Thumbnails** are asked for in batches as rows come into view and kept in the device's cache.

## Keeping folders in sync

A **sync folder** is a name, an id and a list of members; each member is a machine and a path on
it. Every member holds a full copy. There is no primary copy and no hub: any two members that are
on at the same time bring each other up to date, and a member that is on more often (a small
always-on machine) simply makes that happen sooner for everyone.

### What it promises

- Two members that can reach each other and are left alone end up with the same files (on mixed
  operating systems, every representable file converges identically while unrepresentable names are
  held back and reported as problems).
- A change travels through any chain of members: A to an always-on machine while B is off, and
  on to B when B comes on while A is off.
- A file deleted on one member is moved to the bin on the others, not erased. The bin
  (`.legion-sync/bin` inside the folder) uses random per-entry ids unique across restarts and is
  emptied after 30 days by default, never expiring while the local clock is implausible. The bin
  holds only verified bin content: if a program with an already-open handle writes to a displaced
  file after it enters the bin (detected cheaply by size and time on scan, and always verified by
  hash before restore or expiry), it is treated as a save and promoted to a conflict copy beside
  its original path — never refused on restore and never deleted by expiry. If two members pick
  opposite sides of a conflict while apart, one version wins the filename and the other moves to
  the bin on both machines, where it remains recoverable.
- If two members changed the same file while apart, both versions are kept: the newer keeps the
  name, the other becomes `name (changed on <machine> <date>, <hash6>).ext` as a new, independent
  file, and the folder says it has a file with two versions until someone picks. Sync never
  replaces a live conflict copy with different content without a person acting.
- A file deleted on one member and changed on another comes back changed.
- A deletion is remembered as long as one member that saw it still has its index or its folder's
  `.legion-sync`. A member that was off for months — or even two newly joining or restored members
  carrying the same old copy who meet each other before meeting the deleter — does not bring
  deleted files back while any member remembers the deleted hash, while an intentional new save or
  restore on a member creates a newer version that syncs out normally.
- Scanning never invents deletions for unreadable directories, an unmounted or replaced folder
  (checked via `.legion-sync/marker.json`), a damaged or missing index beside an existing folder
  (which stops cleanly with `"This folder's sync record is damaged. Nothing was changed."` until
  rebuilt explicitly from disk), or a mass disappearance (outbound moves are recognized, unmatched
  deletions settle and are counted over a rolling window, and both outbound and inbound mass
  deletions pause under a deletion brake to ask a person how to proceed once for the whole batch).
- Joining with files already on disk (a new member with an old copy, a restored backup, a
  reinstall with files kept, or a removed-and-re-added member) never lets stale files silently
  win or resurrect deletions: equal content merges quietly, known stale versions are replaced and
  moved to the local bin, previously deleted hashes go to the local bin, and unknown content is
  kept alongside.

### How

- **Records and incarnations.** Each member keeps a versioned index of the folder (refusing newer
  index schema versions cleanly, running an integrity check on open, and catching storage panics at
  the engine boundary): per path a record of kind, size, modification time, BLAKE3 hash, whether it
  is deleted, a short ancestry of recent content hashes (up to 16; deletion records keep the hash
  they deleted), and a version vector keyed by `(member, incarnation)`. Opening a missing or empty
  index beside an existing folder marker and non-empty folder (or non-empty deletion log) stops in
  a damaged-index state without touching disk until an explicit rebuild-from-disk recreates the
  index, restores bin sidecars, and replays `.legion-sync/deletions.log`. Each index has a random
  non-zero incarnation id; counters and sequence numbers are per `(member, incarnation)`. A local
  change bumps this member's counter. Comparing two records gives newer, older, equal or
  concurrent; concurrent with equal content merges quietly, concurrent with different content is
  the two-versions case above. Every pull begins with a bidirectional handshake carrying
  `(incarnation, seq, chained_fingerprint)` in both directions before any page moves, backed by a
  dense per-sequence chained digest table pruned only below the minimum direct acknowledgement
  across other members (a peer asking about a pruned sequence is detected as having rolled back).
  A peer presenting a new incarnation is treated as a fresh peer for every purpose: everything
  remembered about its previous life — including its acknowledgements of this member — is retired
  at once so stale acknowledgements never count toward discarding deletions and an honest "no
  position yet" never triggers cascading rotations. Removing and re-adding a member carries an
  explicit rejoin signal in the handshake so healthy members do not rotate. If a member detects
  that it rolled back or reinstalled, it starts a fresh incarnation (carrying existing tombstones,
  held deletions, and `.legion-sync/deletions.log` forward) rather than reusing counters and
  reconciles its disk by the join rule above; peer-forced rotations are rate-limited to at most
  once per day per peer and three times per week in total, after which further claims are ignored
  and reported as a problem naming the peer. Retired `(member, incarnation)` keys are pruned
  deterministically from version vectors once every current member has moved past that incarnation
  and directly acknowledged applied state past every local record mentioning it (this projection
  commutes with version-vector merging and preserves convergence), with a safety cap of 64
  incarnations per member reported durably on both sides (recoverable by removing and re-adding the
  member). Two live machines using the same member id (alternating incarnations or diverging at the
  same sequence) are detected and reported as a problem rather than synced.
- **Direct acknowledgements.** Each member tracks two numbers per peer: its pull position (`seq`)
  and its applied acknowledgement (`applied_ack_seq`), which never advances past the oldest
  unapplied record (whether held back, deferred, invalid, locally unplaceable, or waiting on an
  unknown member). Each member reports only what it has itself pulled and applied from the caller
  (never third-party hearsay about another member). Unapplied records, deferrals, and problems are
  durable across restarts; a syntactically valid member name not yet in the local member list is
  treated as "not yet" (held back unacknowledged and retried on open, membership changes, and
  pulls) rather than discarded as invalid. A deletion record is discarded — and
  `.legion-sync/deletions.log` compacted — only after every other current member has directly
  acknowledged applied state past this member's `(incarnation, seq)`. Deleted content hashes and
  their deletion version vectors are also kept in a durable deleted-history table (evicted
  oldest-first at its 100,000-path bound) so any member carrying an old copy with a deleted hash —
  including two joiners or restored machines that met before the deleter — has that old copy moved
  to the local bin rather than resurrecting the file, while a fresh deliberate save or bin restore
  creates a strictly newer version that syncs out normally.
- **Scanning and the deletion brake.** Before every scan and apply, the engine checks
  `.legion-sync/marker.json` (folder id, member id, incarnation); a missing or mismatched marker
  with a non-empty index stops that member with a readable problem and records no deletions. A
  member notices its own changes by watching the folder (inotify, FSEvents,
  `ReadDirectoryChangesW`, rescanned incrementally by subtree) and by a full scan on start and
  every so often (with an optional full-checksum pass). Every scan also checks bin entries for
  late writes through open handles and promotes any modified bin file into a conflict copy beside
  its original path. A file still being written (changing between two looks) waits. Nothing under
  an unreadable or skipped path is ever recorded as deleted, and sync of that subtree waits. The
  outbound deletion brake is move-aware: same-scan and cross-scan moves matched by content hash do
  not count as deletions, unmatched deletions on brake-eligible folders wait 30 seconds to settle
  before emitting tombstones (and are written immediately to `.legion-sync/deletions.log` marked
  as held so an index loss during the settle window never forgets them), and distinct deletions are
  counted across a 10-minute rolling window (catching mass deletions split across subtree scans).
  If the window crosses more than 50 files and more than a quarter of the folder's live files, the
  member pauses and asks a person either to confirm the deletions or to rescan. A matching inbound
  peer deletion brake checks incoming deletions over the same rolling window before touching local
  disk: when it trips, the receiver counts ahead through the peer's pending pages to report the
  full batch size and asks once for the whole batch (whether to apply the deletions or keep this
  machine's files across all counted pages; later deletions are judged by the window as usual).
- **Pulling and safe replacement.** A member handshakes (exchanging pulled/acked positions,
  retired-incarnation pruning sets, known active peer incarnations, and a bidirectional summary of
  any records one side could not accept from the other) and asks a peer for the records it has not
  seen (`sync.index`, by `(incarnation, seq)`), validates and bounds every page and record (invalid
  or unauthorized records — including a peer forging this member's own counter above its local
  clock or counters above `2^62` — are refused, reported durably, and never acknowledged; conflict
  copy member names come strictly from the folder's member list; conflict status is derived locally
  rather than trusted from peers). Before fetching a file, the engine checks available free space
  against the declared size plus a safety margin; if it will not fit, the download does not start
  and the single record is held back with a plain-language space problem while unrelated records
  continue syncing. Any single record that cannot be placed for a local reason (disk full,
  cross-device error, permission denied, or unsupported local path) is similarly set aside as
  unapplied without advancing the acknowledgement past it, never blocking unrelated records after
  it. Files stream through the bulk pipe into a write-ahead registered staging file — inside
  `.legion-sync/tmp` when on the same filesystem, or beside the destination under a fixed-ignored
  `.legion-*.tmp` name when the destination lives on a separate mount or subvolume — with a
  60-second **idle** timeout (reset on every byte received), cancellation, ranged resume, and a
  hard read cap of declared size plus one byte. If a peer's durable held-back list reaches its
  per-peer cap (4,096 entries), pulling from only that peer stops with a durable problem naming it
  rather than dropping entries; deferrals reaching their retry limit are promoted to durable
  unapplied held-back records rather than dropped or acknowledged. A delete plus create sharing an
  already-local hash is applied as a local rename with no download; clean new files in a page are
  group-committed per page. Target timestamps and permissions are set on the staged file before
  placement. Every replace or delete uses non-replacing renames or atomic swaps (`renameat2` with
  `RENAME_NOREPLACE`/`RENAME_EXCHANGE`, `renamex_np`, `ReplaceFileW`/`MoveFileExW`, with a
  claim-then-rename fallback) with no blind swap-back: pre-checks use fast stat comparisons and any
  displaced file is verified by hash in at most two disk reads (typically one) and moved to
  `.legion-sync/bin` (cross-device safe when the file lived on a separate mount) if it matches the
  indexed version or kept as a conflict copy beside its path if it is an unseen save. Post-placement
  file stats are recorded only when live file identity still matches the staged file (otherwise the
  path is marked for mandatory re-hashing on the next scan). Every mutation — including bin restore
  and conflict resolution — is bracketed with write-ahead records, and mid-apply errors recover
  immediately without deleting unknown bytes from staging. A deleted directory holding only fixed
  OS litter (`.DS_Store`, `Thumbs.db`, `desktop.ini`) moves that litter to the bin and removes the
  directory; one holding folder-ignored or held-back files stays, is reported once, and the cursor
  advances. Only pulling means a node never writes to another node's disk.
- **Serving.** A node serves a sync folder's records and files only to a fleet machine that the
  folder's definition lists as a member, only inside that folder's path, only while its folder
  marker is valid, and never the bin or the temporary directory. Before streaming a file, serving
  verifies the file's on-disk kind, size, and modification time against the index; if they differ —
  or when a fetcher reports a hash or size mismatch for bytes it received — the serving member
  queues that path for mandatory re-hashing on the next scan rather than repeatedly serving stale
  bytes.
- **Who may set it up.** `sync.toml` is a shared document merged entry by entry like
  `favorites.toml`. Creating a folder or adding a member is allowed when the caller may write the
  member's path on that machine; each node checks that again for its own path before it takes
  part, from its own copy of the policy. Removing a member stops syncing there and leaves the
  files. A sync folder may not be created inside another sync folder or contain one.
- **The runner.** Syncing needs something running. `legion sync run` is a long-lived process per
  node (a systemd user unit in the packages, a launchd agent, a scheduled task at logon on
  Windows), started by the first `sync.create` or `sync.add` that involves the node and stopped
  when it has no folders. It watches, scans, pulls from each reachable member with backoff for
  the ones that are off, and carries `favorites.toml` and `sync.toml` along. On start (boot,
  wake, network back) it reconciles at once.
- **What is left out of a copy.** The fixed ignore list is strictly `.legion-sync` at any depth
  (under case folding and Unicode NFKC normalization), OS litter (`.DS_Store`, `Thumbs.db`,
  `desktop.ini`), and files Legion itself is still writing (`*.legion-part`, `.legion-*.tmp`).
  `.git` and editor or download temporary files are not fixed-ignored; put them in the folder's
  own `ignore` globs when desired. Only relative symbolic links whose resolved target stays
  strictly inside the sync folder (and outside `.legion-sync`) are synced between Unix members;
  absolute, escaping, or internal-targeting symbolic links — and all symbolic links on Windows —
  are held back and listed as problems both outbound and inbound. A name the destination cannot
  hold (case or Unicode normalization clash, Windows reserved names or forbidden characters,
  over-long paths) is held back and listed as a problem instead of being mangled, and the same path
  checks apply to bin restore and conflict resolution destinations.
- **Limits and caveats said plainly.** Whole files are sent, not changed blocks. A phone is not a
  member in this version; it browses, plays and downloads from the members. If two members resolve
  the same conflict to opposite choices while apart, one version wins the filename and the other
  moves to `.legion-sync/bin` on both machines (still recoverable from the bin). A deletion is
  remembered as long as one member that saw it still has its index or its folder's `.legion-sync`
  (up to the oldest-first 100,000-path deleted-history bound after tombstone collection); if every
  member loses both or an ancient deletion is evicted from the bounded history, a later member
  carrying an old copy can re-introduce the file. Peer-forced incarnation rotations are rate-limited
  to at most once per day per peer and three times per week in total so a misbehaving peer cannot
  trigger unbounded rotations. Retired incarnations are pruned deterministically once acknowledged
  by all current members; if a record ever still exceeds the 64-incarnation safety cap per member,
  both sides report a durable problem resolved by removing and re-adding the affected member.
  Because tombstone collection requires direct applied acknowledgements from every current member,
  in topologies where two members never connect directly (for example a strict chain where A and C
  only ever talk through B) their deletion records remain in the index until they meet or a member
  is removed. Per-path ancestry keeps the last 16 content hashes, so a joining member carrying a
  copy more than 16 edits behind a live file treats it as unknown content and preserves both as a
  conflict copy rather than silently overwriting anything. On filesystems without an atomic
  file-exchange call, the claim-then-rename fallback briefly leaves the replaced path absent
  between claiming the old file and renaming the new one into place (a concurrent creation in that
  gap is caught by non-replacing rename without data loss).

The engine is its own crate, `legion-sync`, with the file system and the peer behind traits, so
it can be driven by a simulator: many members, mixed OS rules (case-insensitive and Unicode
normalizing), unreadable directories, permission/disk-full/cross-device/commit faults, human edits
mid-apply, late writes through open handles into binned files, hostile peers and lying regression
claims, backup restores, reinstalls, lifetimes of rotations with deterministic version-vector
pruning, duplicate ids, clock jumps, partitions, index loss during the deletion hold, and crashes
(including during recovery), checking convergence, no silent loss, no zombie resurrection,
conflict-copy immutability, sequence monotonicity, and no deletions under skipped paths.

### Status in words

`sync.status` reports per member one of: up to date, syncing (files and bytes left), waiting for
a machine that is off, paused, or a problem with the sentence that explains it. A member with pull
errors, unapplied held-back records, or records refused by a peer reports a problem rather than
"up to date" or merely "syncing", and stale problems clear automatically once resolved. The
interface shows exactly those.

## Order of work

1. Looking and the content pipe: categories, `fs.list` and `fs.find` additions, `fs.thumb`,
   `get` with `length`, `legion-content`, the door's endpoint, `Link::open_content`.
2. The Files tab against those, in the browser door and the app; history-based back.
3. Moving: `fs.transfer` additions, downloads and uploads on the device, Send to, Activity rows.
4. Finding and favourites: search across machines, `fav.*`, pins.
5. Sync: the engine and its simulator, then the runner, the tools and the screens.

## Not in this round

Converting video for devices that cannot decode it; sending only the changed parts of a large
file; the phone as a sync member ("keep on this phone"); sharing a file with someone outside the
fleet; per-person favourites; full-text search of file contents beyond `fs.grep`.
