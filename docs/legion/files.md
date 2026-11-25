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

The table is by extension only, case-insensitive; nothing is opened to guess. `fs.list`,
`fs.find` and `fs.stat` report `category` and `mime` for regular files. The existing `kind`
(`file`, `dir`, `symlink`, `other`) keeps its meaning.

## Tools

Additions are optional fields and new tools, so an older client keeps working against a newer
node.

### Looking

- `fs.list` gains `sort` (`name`, `modified`, `size`), `order` (`asc`, `desc`), `categories`
  (list, keeps directories), `offset`, and reports `total` and `truncated`. Directories still
  come first.
- `fs.find` gains `name` (words that must all appear in the file name, case-insensitive),
  `categories`, `min_size`, `max_size`, `modified_after`, `modified_before`, `sort`, `order`,
  `hidden` (separate from `ignore`, so a search can skip dot files without obeying `.gitignore`)
  and `budget_ms` (default 10 000, at most 60 000). A search that runs out of time or reaches
  `limit` returns what it has with `truncated: true` and `reason` (`time` or `limit`). With
  `on: "all"` it is a search of the fleet; machines that are off are reported, not waited for.
- `fs.thumb` (read): `{paths: [1..32], size: 96..512 = 256, fit: "cover" | "contain"}` returns,
  per path, `{mime, content (base64), width, height}` or an error. Pictures are decoded on the
  node (JPEG, PNG, GIF first frame, WebP, BMP; EXIF orientation applied) inside hard limits:
  source at most 64 MiB and 100 megapixels, decoding off the async threads, a bounded number at
  once. Video posters are made with `ffmpeg` when the node has it and reported `unsupported`
  otherwise. Results are cached in the state directory under a key of path, size, modification
  time and parameters, with the cache capped (256 MiB, least recently used first). A thumbnail of
  a file is allowed exactly when `fs.read` of it is.

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

`legion stream`'s `get` gains `length`. Its reply line becomes
`{size, offset, length, mtime}` followed by exactly `length` bytes (to the end when absent).
`legion-link` gains `Link::open_content(machine, path, offset, length)`, which opens that
operation on the session the link already has for that machine (a new exec channel on the ssh
connection, or a child for a local node) and returns the size, modification time and an async
reader. It follows the routing rules calls follow: it names its machine and never goes somewhere
else.

### Content addresses

An `<img>` or `<video>` cannot send an `Authorization` header and must not run inside the
interface's origin. So content is served from a **content address**: a URL holding a ticket.

- A ticket is 256 random bits, made for one principal, one machine and one path. It is checked as
  `fs.read` when made and again on every use, lives 15 minutes after its last use and 6 hours at
  most, dies with the token or pairing it was made under, and each holder has a bounded number.
- The address answers `GET` and `HEAD` with single-range support (`Range`, `If-Range`, an `ETag`
  from size and modification time, `206` and `416` done properly), streaming with backpressure
  and never reading a file into memory.
- Every reply carries `X-Content-Type-Options: nosniff`,
  `Cross-Origin-Resource-Policy`, `Referrer-Policy: no-referrer` and `Cache-Control: private,
  no-store`. `Content-Type` comes from a short allow-list of types that are safe to show in place
  (raster images, SVG, video, audio, PDF, and `text/plain; charset=utf-8` for every text file,
  including HTML). Everything not on the list is `application/octet-stream` with
  `Content-Disposition: attachment`. Everything except PDF carries
  `Content-Security-Policy: sandbox; default-src 'none'`. The test that matters: an HTML or SVG
  file opened through a content address never runs script with access to the interface.
- `?download=1` forces `Content-Disposition: attachment` with the file's name.

One module (crate `legion-content`) holds the ticket table, the range handling and the headers,
over a small trait that opens a byte range. Two things mount it:

- **The HTTP door**: `POST /v1/content` (bearer token) with `{on, path}` returns
  `{url, expires_in}`; the URL is `/v1/content/<ticket>/<file name>` on the same origin. The door
  reads the file through the node, so a file on another machine comes over the node's own
  session to it.
- **The app**: the native side mounts the same module on a listener bound to `127.0.0.1` on a
  port the system picks, reading through `Link::open_content`. The interface asks the native
  command `content_url({machine, path})` for an address. The ticket in the path is the only
  thing another program on the device would need and cannot guess; tickets are read-only, short
  lived and exist only for files the person opened. Where a platform's web view refuses loopback
  media from the app's origin, the same module is mounted on the app's custom scheme instead,
  answering open-ended ranges in bounded pieces.

The interface's content security policy grows by exactly what this needs: `img-src` and
`media-src` for the content origin.

### To and from the device

- **Download to this device** (app): a native command streams the file to the device's Downloads
  (Android: the shared Downloads collection, in a `Legion` folder; desktop: the user's Downloads
  folder; iOS and iPadOS: the app's folder in Files). It resumes from a partial file, checks the
  finished file against `fs.hash`, reports progress, and shows up in Activity as a row that can be
  opened. In a browser it is a link to the content address with `?download=1`.
- **Open in another app** (app): downloads to the app's cache and hands the file to the system's
  chooser.
- **Add from this device** (app): the system's file picker, then the bulk pipe's `put`, with the
  same resume and hash check.

Nothing here makes a person wait. Starting a download, an upload or a transfer returns at once;
the interface stays free to browse, play something else or leave Files, and the work shows in
Activity. A viewer can be closed while it is still loading, which stops the loading.

Transfers to and from the device run in a queue on the app's native side, not in the web view,
so they do not depend on a screen being open:

- **Android**: a foreground service of the data-sync kind with one ongoing notification (what is
  moving, how far, Pause and Cancel). It keeps going when the person switches apps, locks the
  screen or swipes Legion away. The queue is written to disk; after the process is killed, the
  phone restarts or the connection drops, each transfer resumes from its partial file the next
  time the service can run, and a finished one leaves a notification that opens the file.
- **Desktop**: the same queue. Closing the window while it is working leaves the app running
  without a window until the queue is empty, and says so; quitting outright asks first. What was
  unfinished resumes on the next start.
- **iPhone and iPad**: the system gives an app only a short time in the background, so a transfer
  pauses when the app is left and resumes when it is opened again. The interface says that
  instead of pretending otherwise.

A few transfers run at once (two by default), the rest wait their turn, and a setting keeps large
transfers for Wi-Fi.

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
  emptied after 30 days by default, never expiring while the local clock is implausible. If two
  members pick opposite sides of a conflict while apart, one version wins the filename and the
  other moves to the bin on both machines, where it remains recoverable.
- If two members changed the same file while apart, both versions are kept: the newer keeps the
  name, the other becomes `name (changed on <machine> <date>, <hash6>).ext` as a new, independent
  file, and the folder says it has a file with two versions until someone picks. Sync never
  replaces a live conflict copy with different content without a person acting.
- A file deleted on one member and changed on another comes back changed.
- A deletion is remembered as long as one member that saw it still has its index or its folder's
  `.legion-sync`. A member that was off for months does not bring deleted files back while any
  member remembers the deletion.
- Scanning never invents deletions for unreadable directories, an unmounted or replaced folder
  (checked via `.legion-sync/marker.json`), a damaged or missing index beside an existing folder
  (which stops cleanly with `"This folder's sync record is damaged. Nothing was changed."` until
  rebuilt explicitly from disk), or a mass disappearance (outbound moves are recognized, unmatched
  deletions settle and are counted over a rolling window, and both outbound and inbound mass
  deletions pause under a deletion brake to ask a person how to proceed).
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
  If a member detects that it rolled back or reinstalled, it starts a fresh incarnation (carrying
  existing tombstones and `.legion-sync/deletions.log` forward) rather than reusing counters and
  reconciles its disk by the join rule above; peer-forced rotations are rate-limited to at most
  once per hour per peer and reported as problems. Two live machines using the same member id
  (alternating incarnations or diverging at the same sequence) are detected and reported as a
  problem rather than synced.
- **Direct acknowledgements.** Each member tracks two numbers per peer: its pull position (`seq`)
  and its applied acknowledgement (`applied_ack_seq`), which never advances past the oldest
  unapplied record (whether held back, deferred, invalid, or waiting on an unknown member). Each
  member reports only what it has itself pulled and applied from the caller (never third-party
  hearsay about another member). Unapplied records, deferrals, and problems are durable across
  restarts; a syntactically valid member name not yet in the local member list is treated as "not
  yet" (held back unacknowledged and retried on open, membership changes, and pulls) rather than
  discarded as invalid. A deletion record is discarded — and `.legion-sync/deletions.log` compacted
  — only after every other current member has directly acknowledged applied state past this
  member's `(incarnation, seq)`. Deleted content hashes are also kept in a durable deleted-history
  table so a later re-added member with an old copy cannot resurrect a file whose tombstone was
  already collected.
- **Scanning and the deletion brake.** Before every scan and apply, the engine checks
  `.legion-sync/marker.json` (folder id, member id, incarnation); a missing or mismatched marker
  with a non-empty index stops that member with a readable problem and records no deletions. A
  member notices its own changes by watching the folder (inotify, FSEvents,
  `ReadDirectoryChangesW`, rescanned incrementally by subtree) and by a full scan on start and
  every so often (with an optional full-checksum pass). A file still being written (changing
  between two looks) waits. Nothing under an unreadable or skipped path is ever recorded as
  deleted, and sync of that subtree waits. The outbound deletion brake is move-aware: same-scan and
  cross-scan moves matched by content hash do not count as deletions, unmatched deletions on
  brake-eligible folders wait 30 seconds to settle before emitting tombstones, and distinct
  deletions are counted across a 10-minute rolling window (catching mass deletions split across
  subtree scans). If the window crosses more than 50 files and more than a quarter of the folder's
  live files, the member pauses and asks a person either to confirm the deletions or to rescan. A
  matching inbound peer deletion brake checks incoming deletions over the same rolling window
  before touching local disk: if one peer's changes would delete above the threshold, only that
  peer is paused while a person chooses whether to apply the deletions or keep this machine's files
  (re-asserting them as newer).
- **Pulling and safe replacement.** A member handshakes and asks a peer for the records it has not
  seen (`sync.index`, by `(incarnation, seq)`), validates and bounds every page and record (invalid
  or unauthorized records — including a peer forging this member's own counter above its local
  clock or counters above `2^62` — are refused, reported durably, and never acknowledged; conflict
  copy member names come strictly from the folder's member list; conflict status is derived locally
  rather than trusted from peers), and fetches each file through the bulk pipe into
  `.legion-sync/tmp` (proven by a write-ahead record before creation) with timeouts, cancellation,
  ranged resume, and a hard read cap of declared size plus one byte. If the durable held-back list
  reaches its cap (4,096 entries), pulling from that peer stops with a durable problem rather than
  dropping entries; deferrals reaching their retry limit are promoted to durable unapplied
  held-back records rather than dropped or acknowledged. A delete plus create sharing an
  already-local hash is applied as a local rename with no download; clean new files in a page are
  group-committed per page. Target timestamps and permissions are set on the staged file in
  `.legion-sync/tmp` before placement. Every replace or delete uses non-replacing renames or atomic
  swaps (`renameat2` with `RENAME_NOREPLACE`/`RENAME_EXCHANGE`, `renamex_np`,
  `ReplaceFileW`/`MoveFileExW`, with a claim-then-rename fallback) with no blind swap-back: any
  displaced file is verified by hash and moved to `.legion-sync/bin` if it matches the indexed
  version or kept as a conflict copy beside its path if it is an unseen save. Post-placement file
  stats are recorded only when live file identity still matches the staged file (otherwise the path
  is marked for mandatory re-hashing on the next scan). Every mutation — including bin restore and
  conflict resolution — is bracketed with write-ahead records, and mid-apply errors recover
  immediately without deleting unknown bytes from `.legion-sync/tmp`. A deleted directory holding
  only fixed OS litter (`.DS_Store`, `Thumbs.db`, `desktop.ini`) moves that litter to the bin and
  removes the directory; one holding folder-ignored or held-back files stays, is reported once, and
  the cursor advances. Only pulling means a node never writes to another node's disk.
- **Serving.** A node serves a sync folder's records and files only to a fleet machine that the
  folder's definition lists as a member, only inside that folder's path, only while its folder
  marker is valid, and never the bin or the temporary directory.
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
  remembered as long as one member that saw it still has its index or its folder's `.legion-sync`;
  if every member loses both, a later re-added member with an old copy can re-introduce the file.
  Peer-forced incarnation rotations are rate-limited to at most once per hour per peer so a
  misbehaving peer cannot trigger unbounded rotations. Because tombstone collection requires direct
  applied acknowledgements from every current member, in topologies where two members never connect
  directly (for example a strict chain where A and C only ever talk through B) their deletion
  records remain in the index until they meet or a member is removed. Per-path ancestry keeps the
  last 16 content hashes, so a joining member carrying a copy more than 16 edits behind a live file
  treats it as unknown content and preserves both as a conflict copy rather than silently
  overwriting anything. On filesystems without an atomic file-exchange call, the claim-then-rename
  fallback briefly leaves the replaced path absent between claiming the old file and renaming the
  new one into place (a concurrent creation in that gap is caught by non-replacing rename without
  data loss).

The engine is its own crate, `legion-sync`, with the file system and the peer behind traits, so
it can be driven by a simulator: many members, mixed OS rules (case-insensitive and Unicode
normalizing), unreadable directories, permission/disk-full/cross-device/commit faults, human edits
mid-apply, hostile peers, backup restores, reinstalls, duplicate ids, clock jumps, partitions,
and crashes (including during recovery), checking convergence, no silent loss, no zombie
resurrection, conflict-copy immutability, sequence monotonicity, and no deletions under skipped
paths.

### Status in words

`sync.status` reports per member one of: up to date, syncing (files and bytes left), waiting for
a machine that is off, paused, or a problem with the sentence that explains it. The interface
shows exactly those.

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
