/**
 * Optional file and device-transfer methods on a Legion `link`.
 *
 * The core link surface (`kind`, `state`, `tools`, `call`) is always present. The methods below
 * are optional capabilities that each host implements when available so the UI can feature-detect
 * them cleanly without breaking against older nodes or hosts that have not implemented them yet.
 *
 * Every transfer method returns control immediately (non-blocking) with a transfer id; progress
 * runs in the background (native foreground service on Android, background queue on desktop, or
 * browser download in HTTP mode) and appears in Activity.
 *
 * @typedef {Object} ContentUrlRequest
 * @property {string} machine - Fleet machine holding the file.
 * @property {string} path - Full path on that machine.
 * @property {boolean} [download=false] - When true, forces `Content-Disposition: attachment`.
 * @property {AbortSignal} [signal] - Optional abort signal so closing a viewer stops loading.
 *
 * @typedef {Object} ContentUrlResult
 * @property {string} url - Ticketed content address for `<img>`, `<video>`, `<audio>`, or download.
 * @property {number | null} [expires_in] - Ticket lifetime in seconds.
 *
 * @typedef {'queued' | 'running' | 'paused' | 'cancelling' | 'done' | 'failed' | 'cancelled'} DeviceTransferState
 *
 * @typedef {Object} DeviceTransfer
 * @property {string} id - Stable transfer identifier.
 * @property {'download' | 'upload'} direction - Whether bytes move to or from this device.
 * @property {string} machine - Peer machine name.
 * @property {string} path - Remote path on `machine`.
 * @property {string} name - Display file name.
 * @property {string | null} [localPath] - Device destination/source path (shown in technical mode).
 * @property {string | null} [mime] - File MIME type when known.
 * @property {DeviceTransferState} state - Current queue state.
 * @property {number} bytes_done - Bytes transferred so far.
 * @property {number} bytes_total - Total file size in bytes.
 * @property {number | null} [rate] - Current transfer speed in bytes/second.
 * @property {{ code?: string, message: string, hint?: string, reason?: string } | string | null} [error] - Reason when failed.
 * @property {string} created - ISO-8601 creation timestamp.
 * @property {string | null} [finished] - ISO-8601 completion timestamp when terminal.
 * @property {string | null} [hash] - Verified BLAKE3 hash (shown in technical mode).
 * @property {boolean} [canOpen] - Whether tapping the finished row opens the local file.
 * @property {{ code?: string, message: string, hint?: string, reason?: string } | null} [openError] - Error if opening externally failed after finishing.
 * @property {boolean} [openExternally] - True when started by "Open in another app" into the cache.
 *
 * @typedef {Object} ContentUrlStatus
 * @property {number} request_count - Number of requests served for this ticket URL.
 * @property {number | null} [status] - Most recent HTTP status code.
 * @property {string | null} [content_type] - Most recent response Content-Type header.
 * @property {boolean} stream_failed - True if any streaming body aborted mid-stream.
 * @property {string | null} [error_code] - Structured Legion error code from an error body.
 * @property {string | null} [error_message] - Error message from an error body.
 * @property {boolean} ticket_live - True while the ticket is live in the table.
 *
 * @typedef {Object} OptionalFileLinkMethods
 *
 * @property {(req: ContentUrlRequest) => Promise<ContentUrlResult>} [contentUrl]
 *   Mint a short-lived read-only ticketed content address for viewing or browser download.
 *   In the HTTP door this calls `POST /v1/content`; in the native app it calls `content_url`.
 *
 * @property {(url: string) => Promise<ContentUrlStatus | null>} [contentStatus]
 *   Inspect the local content server's observed HTTP status and stream health for `url`.
 *
 * @property {(req: { machine: string, path: string, name?: string, size?: number, mime?: string }) => Promise<{ id: string }>} [download]
 *   Enqueue a non-blocking download to this device's Downloads folder and return `{ id }` at once.
 *   In the browser HTTP link, opens/triggers a download link to `contentUrl({ ..., download: true })`.
 *   Hidden in the UI when absent.
 *
 * @property {(req: { machine: string, dir: string, files?: Array<{ name: string, size?: number, path?: string }> }) => Promise<{ id: string, ids?: string[], count?: number } | null>} [upload]
 *   Open the device's file picker (or accept pre-selected files in tests), enqueue upload(s) to
 *   `machine:dir` via the bulk pipe, and return `{ id }` at once. Returns `null` if cancelled.
 *   Hidden in the UI when absent.
 *
 * @property {(req: { machine: string, path: string, name?: string, size?: number, mime?: string }) => Promise<{ id?: string, ok?: boolean, openedImmediately?: boolean }>} [openExternally]
 *   Hand a remote file to the device's system chooser (downloading to cache on native or reusing
 *   an unchanged cached copy immediately). Hidden in the UI when absent.
 *
 * @property {() => Promise<DeviceTransfer[]>} [transfers]
 *   List queued, running, paused, finished, and failed device-side transfers for Activity.
 *
 * @property {(id: string, action: 'pause' | 'resume' | 'cancel') => Promise<DeviceTransfer>} [transferControl]
 *   Pause, resume (from partial file), or cancel a device-side transfer.
 *
 * @property {(id: string) => Promise<{ ok?: boolean, viewer?: { machine: string, path: string } } | void>} [openTransfer]
 *   Open a finished download on this device (native viewer/chooser, or returns viewer route in mock).
 *
 * @property {(listener: (transfers: DeviceTransfer[]) => void) => () => void} [onTransferEvent]
 *   Subscribe to live device transfer queue progress and state updates. Returns an unsubscribe fn.
 */

export const DEVICE_TRANSFER_STATES = Object.freeze([
  'queued',
  'running',
  'paused',
  'cancelling',
  'done',
  'failed',
  'cancelled',
]);

export const DEVICE_TRANSFER_ACTIONS = Object.freeze(['pause', 'resume', 'cancel']);

/**
 * Checks whether a link implements the native/mock device transfer queue API.
 */
export function hasDeviceTransferQueue(link) {
  return Boolean(link && typeof link.transfers === 'function');
}
