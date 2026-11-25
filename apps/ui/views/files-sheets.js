// Sheets for Files: Send to (with destination folder picker), Sort, Folder More, New folder,
// Rename, Search More filters, and Sync folder actions.

import { html, useEffect, useMemo, useState } from '../vendor/preact-htm.js';
import { Icon } from '../icons.js';
import {
  baseFileName,
  buildTransferDestination,
  folderDisplayName,
  formatPathForDisplay,
  joinFilePath,
  parentFilePath,
} from '../lib/files.js';
import { readRecentDestinations, recordRecentDestination } from '../lib/files-store.js';
import { formatBytes, formatRelativeTime, primaryDisk } from '../lib/format.js';
import { Notice, Sheet } from './common.js';

function safeStorage() {
  try {
    return window.localStorage;
  } catch {
    return null;
  }
}

function freeDiskPhrase(machine) {
  const disk = primaryDisk(machine?.sys?.disks);
  if (!disk?.disk?.available) return '';
  return `${formatBytes(disk.disk.available)} free`;
}

// ── Send to sheet ────────────────────────────────────────────────────────────────────────────

export function SendToSheet({
  sourceMachine,
  items = [],
  machines = [],
  favourites = [],
  caps,
  capsStore = null,
  folderChanges = null,
  allowedSet,
  wakePendingSet = null,
  deviceNoun = 'phone',
  technical = false,
  link,
  onEnsureAwake,
  onRefreshFleet,
  onLocalDone,
  onStarted,
  onClose,
}) {
  const [mode, setMode] = useState('copy'); // 'copy' | 'move'
  const [, setCapsRev] = useState(0);
  const canWake = allowedSet ? allowedSet.has('power.wake') : false;

  useEffect(() => {
    if (!capsStore) return undefined;
    const unsub = capsStore.subscribe(() => setCapsRev((n) => n + 1));
    if (sourceMachine) capsStore.ensure(sourceMachine).catch(() => {});
    return unsub;
  }, [capsStore, sourceMachine]);

  const sourceCaps = useMemo(
    () => (capsStore && sourceMachine ? capsStore.fileCapabilities(sourceMachine, { fallbackAllowedSet: allowedSet }) : caps),
    [capsStore, sourceMachine, allowedSet, caps, capsStore ? capsStore.getRevision() : 0],
  );

  // Order machines: online other machines first, then off machines, then source machine last.
  const orderedMachines = useMemo(() => {
    const onlineOthers = machines.filter((m) => m.reachable && m.name !== sourceMachine);
    const offline = machines.filter((m) => !m.reachable);
    const sourceOnline = machines.filter((m) => m.reachable && m.name === sourceMachine);
    return [...onlineOthers, ...offline, ...sourceOnline];
  }, [machines, sourceMachine]);

  const defaultDest = useMemo(() => {
    const firstOnlineOther = orderedMachines.find((m) => m.reachable && m.name !== sourceMachine);
    if (firstOnlineOther) return firstOnlineOther.name;
    const firstOnline = orderedMachines.find((m) => m.reachable);
    if (firstOnline) return firstOnline.name;
    if (caps?.hasDownload) return '__device__';
    return '';
  }, [orderedMachines, sourceMachine, caps?.hasDownload]);

  const [dest, setDest] = useState(defaultDest);
  const [destDirByMachine, setDestDirByMachine] = useState({});
  const [pickingFolder, setPickingFolder] = useState(false);
  const [browsePath, setBrowsePath] = useState('~');
  const [browseDirs, setBrowseDirs] = useState([]);
  const [browseLoading, setBrowseLoading] = useState(false);
  const [browseError, setBrowseError] = useState(null);

  const isSameMachine = dest === sourceMachine;
  const canMove = isSameMachine
    ? Boolean(sourceCaps?.hasMove || sourceCaps?.hasTransferMove)
    : Boolean(sourceCaps?.hasTransferMove);
  const moveNeedsNewerLegion =
    !isSameMachine &&
    Boolean(sourceCaps?.known) &&
    Boolean(sourceCaps?.hasTransfer) &&
    Boolean(sourceCaps?.hasRemove) &&
    !sourceCaps?.hasTransferMove;

  useEffect(() => {
    if (!canMove && mode === 'move') setMode('copy');
  }, [canMove, mode]);

  // While any offline machine shown in this sheet is waking, poll fleet status so it becomes
  // selectable in place as soon as it answers.
  const hasWakingOffMachine = useMemo(
    () => Boolean(wakePendingSet && machines.some((m) => !m.reachable && wakePendingSet.has(m.name))),
    [machines, wakePendingSet],
  );

  useEffect(() => {
    if (!hasWakingOffMachine || typeof onRefreshFleet !== 'function') return undefined;
    const timer = window.setInterval(() => {
      onRefreshFleet({ background: true, forceOffline: true });
    }, 600);
    return () => window.clearInterval(timer);
  }, [hasWakingOffMachine, onRefreshFleet]);

  const pinnedDirsForDest = useMemo(() => {
    if (!dest || dest === '__device__') return [];
    return (favourites || []).filter((f) => f.machine === dest && f.kind === 'dir' && f.pinned);
  }, [favourites, dest]);

  const recentDirsForDest = useMemo(() => {
    if (!dest || dest === '__device__') return [];
    return readRecentDestinations(safeStorage(), dest);
  }, [dest]);

  const chosenDir = useMemo(() => {
    if (!dest || dest === '__device__') return '';
    if (destDirByMachine[dest]) return destDirByMachine[dest];
    if (pinnedDirsForDest.length > 0) return pinnedDirsForDest[0].path;
    if (recentDirsForDest.length > 0) return recentDirsForDest[0];
    return '/home/me';
  }, [dest, destDirByMachine, pinnedDirsForDest, recentDirsForDest]);

  const chosenDirMeta = useMemo(() => {
    const pinned = pinnedDirsForDest.find((p) => p.path === chosenDir);
    if (pinned) {
      return {
        label: pinned.label || folderDisplayName(chosenDir, { technical }),
        sub: `Pinned · ${formatPathForDisplay(chosenDir, { technical })}`,
      };
    }
    if (recentDirsForDest.includes(chosenDir)) {
      return {
        label: folderDisplayName(chosenDir, { technical }),
        sub: `Recent · ${formatPathForDisplay(chosenDir, { technical })}`,
      };
    }
    return {
      label: folderDisplayName(chosenDir, { technical }),
      sub: formatPathForDisplay(chosenDir, { technical }),
    };
  }, [chosenDir, pinnedDirsForDest, recentDirsForDest, technical]);

  useEffect(() => {
    if (!pickingFolder || !dest || dest === '__device__') return;
    let alive = true;
    setBrowseLoading(true);
    setBrowseError(null);
    const listCall = capsStore
      ? capsStore.call('fs.list', { path: browsePath, depth: 1, hidden: false }, { on: dest }).then((r) => r.value)
      : link.call('fs.list', { path: browsePath, depth: 1, hidden: false }, { on: dest });
    listCall
      .then((res) => {
        if (!alive) return;
        const resolved = res?.path || browsePath;
        if (resolved !== browsePath) setBrowsePath(resolved);
        setBrowseDirs((res?.entries || []).filter((e) => e.kind === 'dir'));
        setBrowseLoading(false);
      })
      .catch((err) => {
        if (!alive) return;
        setBrowseError(err);
        setBrowseLoading(false);
      });
    return () => {
      alive = false;
    };
  }, [pickingFolder, dest, browsePath, capsStore]);

  const allSizesKnown =
    items.length > 0 && items.every((it) => it && it.kind !== 'dir' && Number.isFinite(it.size));
  const totalBytes = allSizesKnown ? items.reduce((sum, it) => sum + it.size, 0) : 0;
  const what =
    items.length === 1
      ? items[0].name || baseFileName(items[0].path)
      : `${items.length} items`;
  const title = `Send ${what}`;
  const subParts = [
    allSizesKnown ? formatBytes(totalBytes) : null,
    `from ${sourceMachine}`,
  ].filter(Boolean);

  const isDeviceDest = dest === '__device__';
  const effectiveMode = isDeviceDest ? 'copy' : mode;
  const confirmLabel = isDeviceDest
    ? `Download to this ${deviceNoun}`
    : dest === sourceMachine
      ? `${effectiveMode === 'move' ? 'Move' : 'Copy'} here`
      : `${effectiveMode === 'move' ? 'Move' : 'Copy'} to ${dest}`;

  function handleConfirm() {
    if (!dest || items.length === 0) return;
    // Close sheet immediately so nothing blocks the user.
    const destMachine = dest;
    const targetDir = chosenDir;
    const isMove = effectiveMode === 'move';
    if (!onStarted) onClose();

    if (destMachine === '__device__') {
      const downloadable = items.filter((item) => item.kind !== 'dir');
      const startedNames = downloadable.map((item) => item.name || baseFileName(item.path));
      const transferIdsPromise =
        typeof link?.download === 'function'
          ? Promise.all(
              downloadable.map((item) =>
                link
                  .download({
                    machine: sourceMachine,
                    path: item.path,
                    name: item.name || baseFileName(item.path),
                    size: item.size,
                    mime: item.mime,
                  })
                  .catch(() => null),
              ),
            ).then((results) => results.map((r) => r?.id).filter(Boolean))
          : Promise.resolve([]);
      if (onStarted) {
        onStarted({
          kind: 'download',
          transferIdsPromise,
          message:
            startedNames.length === 1
              ? `Downloading ${startedNames[0]}. Watch progress in Activity.`
              : `Downloading ${startedNames.length} items. Watch progress in Activity.`,
        });
      }
      return;
    }

    recordRecentDestination(safeStorage(), destMachine, targetDir);

    // Same-machine Move uses `fs.move` and same-machine Copy uses `fs.copy` — instant, no job.
    if (destMachine === sourceMachine && ((isMove && sourceCaps?.hasMove) || (!isMove && sourceCaps?.hasCopy))) {
      const toolName = isMove ? 'fs.move' : 'fs.copy';
      const folderName = folderDisplayName(targetDir, { technical });
      const runLocal = async () => {
        const touchedSources = new Set();
        let anySucceeded = false;
        for (let idx = 0; idx < items.length; idx += 1) {
          const item = items[idx];
          const destPath = buildTransferDestination(targetDir, item.path);
          if (destPath === item.path) continue;
          const callPromise = capsStore
            ? capsStore.call(toolName, { from: item.path, to: destPath }, { on: sourceMachine })
            : link.call(toolName, { from: item.path, to: destPath }, { on: sourceMachine });
          const ok = await callPromise.then(() => true, () => false);
          if (ok) {
            anySucceeded = true;
            const srcParent = parentFilePath(item.path) || '~';
            if (isMove) touchedSources.add(srcParent);
            if (idx < items.length - 1 && folderChanges) {
              folderChanges.changed(sourceMachine, targetDir, {
                throttle: true,
                reason: isMove ? 'move' : 'copy',
              });
              if (isMove) {
                folderChanges.changed(sourceMachine, srcParent, {
                  throttle: true,
                  reason: 'move',
                });
              }
            }
          }
        }
        if (anySucceeded && folderChanges) {
          folderChanges.changed(sourceMachine, targetDir, {
            throttle: false,
            reason: isMove ? 'move' : 'copy',
          });
          for (const srcParent of touchedSources) {
            folderChanges.changed(sourceMachine, srcParent, {
              throttle: false,
              reason: 'move',
            });
          }
        }
        if (onLocalDone) onLocalDone();
      };
      runLocal();
      if (onStarted) {
        onStarted({
          kind: isMove ? 'move' : 'copy',
          local: true,
          message: `${isMove ? 'Moved' : 'Copied'} ${what} to ${folderName}.`,
        });
      }
      return;
    }

    const runTransfers = async () => {
      const startedJobs = [];
      for (const item of items) {
        const destPath = buildTransferDestination(targetDir, item.path);
        const srcDir = parentFilePath(item.path) || '~';
        const baseArgs = {
          from: item.path,
          to: `${destMachine}:${destPath}`,
          background: true,
          if_exists: 'rename',
        };
        if (isMove) baseArgs.move = true;

        const callTransfer = (targetMachine, transferArgs) => {
          if (capsStore) {
            return capsStore
              .call('fs.transfer', transferArgs, {
                on: targetMachine,
                optional: isMove ? ['background', 'if_exists'] : ['background', 'if_exists', 'move'],
              })
              .then((r) => r.value);
          }
          const pruned = { from: transferArgs.from, to: transferArgs.to };
          if (sourceCaps?.hasTransferBg) pruned.background = true;
          if (sourceCaps?.hasTransferIfExists) pruned.if_exists = 'rename';
          if (isMove && sourceCaps?.hasTransferMove) pruned.move = true;
          return link.call('fs.transfer', pruned, { on: targetMachine });
        };

        let res = null;
        try {
          res = await callTransfer(sourceMachine, baseArgs);
        } catch {
          // Fall back to starting the transfer on the destination when the source cannot reach it.
          const fallbackArgs = {
            ...baseArgs,
            from: `${sourceMachine}:${item.path}`,
            to: destPath,
          };
          res = await callTransfer(destMachine, fallbackArgs).catch(() => null);
        }
        if (res?.job?.id) {
          startedJobs.push({
            id: res.job.id,
            machine: res.job.machine || sourceMachine,
            label: item.name || baseFileName(item.path),
            move: isMove,
            destMachine,
            destDir: targetDir,
            srcMachine: sourceMachine,
            srcDir,
          });
        } else if (res && folderChanges) {
          // Synchronous transfer (older machine without background jobs).
          folderChanges.changed(destMachine, targetDir, {
            throttle: false,
            reason: 'transfer',
          });
          if (isMove) {
            folderChanges.changed(sourceMachine, srcDir, {
              throttle: false,
              reason: 'transfer',
            });
          }
        }
      }
      return startedJobs;
    };
    const transferJobsPromise = runTransfers();

    const verb = isMove ? 'Moving' : 'Copying';
    if (onStarted) {
      onStarted({
        kind: 'transfer',
        transferJobsPromise,
        message: `${verb} ${what} to ${destMachine}. Watch progress in Activity.`,
      });
    }
  }

  return html`
    <${Sheet} title=${title} onClose=${onClose}>
      <p class="sheet-sub">${subParts.join(' · ')}</p>

      ${!isDeviceDest
        ? html`
            <div class="seg" role="group" aria-label="Copy or move">
              <button
                type="button"
                aria-pressed=${mode === 'copy' ? 'true' : 'false'}
                onClick=${() => setMode('copy')}
              >
                Copy
              </button>
              <button
                type="button"
                aria-pressed=${mode === 'move' ? 'true' : 'false'}
                disabled=${!canMove}
                onClick=${() => setMode('move')}
              >
                Move
              </button>
            </div>
            ${moveNeedsNewerLegion
              ? html`<p class="sheet-note">${sourceMachine} needs a newer Legion to move files between machines. You can still copy them.</p>`
              : null}
          `
        : null}

      ${pickingFolder && dest && !isDeviceDest
        ? html`
            <p class="label">Choose folder on ${dest}</p>
            ${pinnedDirsForDest.length > 0
              ? html`
                  <div class="list">
                    ${pinnedDirsForDest.map(
                      (p) => html`
                        <button
                          key=${p.id}
                          type="button"
                          class=${chosenDir === p.path ? 'row picked' : 'row'}
                          onClick=${() => {
                            setDestDirByMachine((prev) => ({ ...prev, [dest]: p.path }));
                            setPickingFolder(false);
                          }}
                        >
                          <span class="thumb folder"><${Icon} name="files" /></span>
                          <span class="body">
                            <span class="title">${p.label || baseFileName(p.path)}</span>
                            <span class="sub">Pinned · ${formatPathForDisplay(p.path, { technical })}</span>
                          </span>
                          <span class="check" aria-hidden="true"></span>
                        </button>
                      `,
                    )}
                  </div>
                `
              : null}

            ${recentDirsForDest.length > 0
              ? html`
                  <p class="label">Last used on ${dest}</p>
                  <div class="list">
                    ${recentDirsForDest.map(
                      (dir) => html`
                        <button
                          key=${dir}
                          type="button"
                          class=${chosenDir === dir ? 'row picked' : 'row'}
                          onClick=${() => {
                            setDestDirByMachine((prev) => ({ ...prev, [dest]: dir }));
                            setPickingFolder(false);
                          }}
                        >
                          <span class="thumb folder"><${Icon} name="files" /></span>
                          <span class="body">
                            <span class="title">${folderDisplayName(dir, { technical })}</span>
                            <span class="sub">${formatPathForDisplay(dir, { technical })}</span>
                          </span>
                          <span class="check" aria-hidden="true"></span>
                        </button>
                      `,
                    )}
                  </div>
                `
              : null}

            <p class="label">Browse ${formatPathForDisplay(browsePath, { technical })}</p>
            ${browseError ? html`<${Notice} error=${browseError} />` : null}
            <div class="list">
              ${parentFilePath(browsePath)
                ? html`
                    <button
                      type="button"
                      class="row"
                      onClick=${() => setBrowsePath(parentFilePath(browsePath))}
                    >
                      <span class="thumb folder"><${Icon} name="back" /></span>
                      <span class="body"><span class="title">Up a folder</span></span>
                    </button>
                  `
                : null}
              ${browseLoading
                ? html`<div class="row"><span class="body"><span class="sub">Loading folders…</span></span></div>`
                : browseDirs.length === 0
                  ? html`<div class="row"><span class="body"><span class="sub">No subfolders here.</span></span></div>`
                  : browseDirs.map(
                      (d) => html`
                        <button
                          key=${d.path}
                          type="button"
                          class="row"
                          onClick=${() => setBrowsePath(d.path)}
                        >
                          <span class="thumb folder"><${Icon} name="files" /></span>
                          <span class="body"><span class="title">${d.name}</span></span>
                          <span class="chev" aria-hidden="true">›</span>
                        </button>
                      `,
                    )}
            </div>
            <div class="actions">
              <button type="button" class="btn" onClick=${() => setPickingFolder(false)}>Back</button>
              <button
                type="button"
                class="btn primary"
                onClick=${() => {
                  setDestDirByMachine((prev) => ({ ...prev, [dest]: browsePath }));
                  setPickingFolder(false);
                }}
              >
                Use ${folderDisplayName(browsePath, { technical })}
              </button>
            </div>
          `
        : html`
            <p class="label">To</p>
            <div class="list" role="radiogroup" aria-label="Destination">
              ${orderedMachines.map((m) => {
                if (!m.reachable) {
                  const waking = Boolean(wakePendingSet?.has(m.name));
                  return html`
                    <div key=${m.name} class="row off">
                      <span class="dot off" aria-hidden="true"></span>
                      <span class="body">
                        <span class="title">${m.name}</span>
                        <span class="sub">${waking ? 'Waking…' : 'Off · turn it on to send here'}</span>
                      </span>
                      ${m.canWake && canWake && onEnsureAwake
                        ? html`
                            <button
                              type="button"
                              class="btn small"
                              disabled=${waking}
                              onClick=${() =>
                                onEnsureAwake(m, {
                                  kind: 'transfer',
                                  label: `send ${what} to ${m.name}`,
                                  forgettable: true,
                                  onContinue: () => {
                                    setDest(m.name);
                                  },
                                })}
                            >
                              ${waking ? 'Waking…' : 'Wake'}
                            </button>
                          `
                        : null}
                    </div>
                  `;
                }
                const picked = dest === m.name;
                const isSelf = m.name === sourceMachine;
                const sub = isSelf ? 'Another folder on this machine' : freeDiskPhrase(m) || 'On';
                return html`
                  <button
                    key=${m.name}
                    type="button"
                    role="radio"
                    aria-checked=${picked ? 'true' : 'false'}
                    class=${picked ? 'row picked' : 'row'}
                    onClick=${() => setDest(m.name)}
                  >
                    <span class="dot" aria-hidden="true"></span>
                    <span class="body">
                      <span class="title">${m.name}</span>
                      <span class="sub">${sub}</span>
                    </span>
                    <span class="check" aria-hidden="true"></span>
                  </button>
                `;
              })}
              ${caps?.hasDownload && mode !== 'move'
                ? html`
                    <button
                      type="button"
                      role="radio"
                      aria-checked=${isDeviceDest ? 'true' : 'false'}
                      class=${isDeviceDest ? 'row picked' : 'row'}
                      onClick=${() => setDest('__device__')}
                    >
                      <span class="body">
                        <span class="title">This ${deviceNoun}</span>
                        <span class="sub">Downloads</span>
                      </span>
                      <span class="check" aria-hidden="true"></span>
                    </button>
                  `
                : null}
            </div>

            ${dest && !isDeviceDest
              ? html`
                  <p class="label">Folder on ${dest}</p>
                  <div class="list">
                    <button
                      type="button"
                      class="row"
                      onClick=${() => {
                        setBrowsePath(chosenDir || '~');
                        setPickingFolder(true);
                      }}
                    >
                      <span class="thumb folder"><${Icon} name="files" /></span>
                      <span class="body">
                        <span class="title">${chosenDirMeta.label}</span>
                        <span class="sub">${chosenDirMeta.sub}</span>
                      </span>
                      <span class="chev word">Change</span>
                    </button>
                  </div>
                `
              : null}

            <div class="sheet-cta">
              <button
                type="button"
                class="btn primary"
                disabled=${!dest}
                onClick=${handleConfirm}
              >
                ${confirmLabel}
              </button>
            </div>
          `}
    <//>
  `;
}

// ── Sort sheet ───────────────────────────────────────────────────────────────────────────────

export function SortSheet({ sortPref, kindFilter = '', onChange, onChangeKind = null, onClose }) {
  const options = [
    { sort: 'name', order: 'asc', label: 'Name (A–Z)' },
    { sort: 'name', order: 'desc', label: 'Name (Z–A)' },
    { sort: 'modified', order: 'desc', label: 'Newest first' },
    { sort: 'modified', order: 'asc', label: 'Oldest first' },
    { sort: 'size', order: 'desc', label: 'Largest first' },
    { sort: 'size', order: 'asc', label: 'Smallest first' },
  ];
  const kindOptions = [
    { id: '', label: 'All' },
    { id: 'image', label: 'Pictures' },
    { id: 'video', label: 'Videos' },
    { id: 'audio', label: 'Music' },
    { id: 'document', label: 'Documents' },
    { id: 'text', label: 'Text' },
    { id: 'archive', label: 'Archives' },
  ];

  return html`
    <${Sheet} title=${onChangeKind ? 'Sort and filter' : 'Sort'} onClose=${onClose}>
      <div class="list" role="radiogroup" aria-label="Sort order">
        ${options.map((opt) => {
          const picked = sortPref.sort === opt.sort && sortPref.order === opt.order;
          return html`
            <button
              key=${`${opt.sort}-${opt.order}`}
              type="button"
              role="radio"
              aria-checked=${picked ? 'true' : 'false'}
              class=${picked ? 'row picked' : 'row'}
              onClick=${() => {
                onChange({ sort: opt.sort, order: opt.order });
                onClose();
              }}
            >
              <span class="body"><span class="title">${opt.label}</span></span>
              <span class="check" aria-hidden="true"></span>
            </button>
          `;
        })}
      </div>
      ${onChangeKind
        ? html`
            <p class="label">Kind</p>
            <div class="chips" role="group" aria-label="Filter by kind">
              ${kindOptions.map(
                (opt) => html`
                  <button
                    key=${opt.id || 'all'}
                    type="button"
                    class="chip-btn"
                    aria-pressed=${(kindFilter || '') === opt.id ? 'true' : 'false'}
                    onClick=${() => {
                      onChangeKind(opt.id);
                      onClose();
                    }}
                  >
                    ${opt.label}
                  </button>
                `,
              )}
            </div>
          `
        : null}
    <//>
  `;
}

// ── Folder More sheet ────────────────────────────────────────────────────────────────────────

export function FolderMoreSheet({
  machine,
  path,
  caps,
  deviceNoun = 'phone',
  showHidden,
  isStarred,
  isPinned,
  onStartSelect,
  onNewFolder,
  onUpload,
  onToggleStarFolder,
  onTogglePinFolder,
  onToggleHidden,
  onClose,
}) {
  return html`
    <${Sheet} title=${folderDisplayName(path)} onClose=${onClose}>
      <div class="list">
        <button
          type="button"
          class="row"
          onClick=${() => {
            if (onStartSelect) onStartSelect();
            else onClose();
          }}
        >
          <span class="body">
            <span class="title">Select</span>
            <span class="sub">Pick files to send, download, or delete</span>
          </span>
        </button>

        ${caps?.hasMkdir
          ? html`
              <button
                type="button"
                class="row"
                onClick=${() => {
                  if (onNewFolder) onNewFolder();
                  else onClose();
                }}
              >
                <span class="body">
                  <span class="title">New folder</span>
                  <span class="sub">Create a subfolder here</span>
                </span>
              </button>
            `
          : null}

        ${caps?.hasUpload
          ? html`
              <button
                type="button"
                class="row"
                onClick=${() => {
                  onClose();
                  if (onUpload) onUpload();
                }}
              >
                <span class="body">
                  <span class="title">Add from this ${deviceNoun}</span>
                  <span class="sub">Upload files into this folder</span>
                </span>
              </button>
            `
          : null}

        ${caps?.hasFavWrite
          ? html`
              <button
                type="button"
                class="row"
                onClick=${() => {
                  onClose();
                  if (onToggleStarFolder) onToggleStarFolder();
                }}
              >
                <span class="body">
                  <span class="title">${isStarred ? 'Remove star' : 'Star this folder'}</span>
                  <span class="sub">Keep it in Favourites across your fleet</span>
                </span>
              </button>
              <button
                type="button"
                class="row"
                onClick=${() => {
                  onClose();
                  if (onTogglePinFolder) onTogglePinFolder();
                }}
              >
                <span class="body">
                  <span class="title">${isPinned ? 'Unpin from Files' : 'Pin to Files'}</span>
                  <span class="sub">Show as a tile at the top of Files</span>
                </span>
              </button>
            `
          : null}

        <label class="row">
          <span class="body">
            <span class="title">Show hidden files</span>
            <span class="sub">Files starting with a dot</span>
          </span>
          <span class="switch row-switch">
            <input
              type="checkbox"
              checked=${showHidden}
              onChange=${(e) => onToggleHidden && onToggleHidden(e.target.checked)}
            />
          </span>
        </label>

        <div class="row">
          <span class="body">
            <span class="title">Copy path</span>
            <span class="sub mono">${machine}:${path}</span>
          </span>
          <button
            type="button"
            class="btn small"
            onClick=${async () => {
              try {
                await navigator.clipboard.writeText(path);
              } catch {
                // Ignore clipboard refusal.
              }
              onClose();
            }}
          >
            Copy
          </button>
        </div>
      </div>
    <//>
  `;
}

// ── New folder & Rename sheets ───────────────────────────────────────────────────────────────

export function NewFolderSheet({ machine, parentPath, link, capsStore = null, folderChanges = null, onCreated, onClose }) {
  const [name, setName] = useState('');
  const [error, setError] = useState(null);
  const [busy, setBusy] = useState(false);

  async function submit(e) {
    e.preventDefault();
    const trimmed = name.trim();
    if (!trimmed || trimmed.includes('/') || trimmed.includes('\\')) {
      setError({ message: 'Enter a folder name without slashes.' });
      return;
    }
    setBusy(true);
    setError(null);
    try {
      const target = joinFilePath(parentPath, trimmed);
      if (capsStore) {
        await capsStore.call('fs.mkdir', { path: target, parents: false }, { on: machine, optional: ['parents'] });
      } else {
        await link.call('fs.mkdir', { path: target, parents: false }, { on: machine });
      }
      if (folderChanges) {
        folderChanges.changed(machine, parentPath, { reason: 'mkdir' });
      }
      onClose();
      if (onCreated) onCreated(target);
    } catch (err) {
      setError(err);
      setBusy(false);
    }
  }

  return html`
    <${Sheet} title="New folder" onClose=${onClose}>
      <form onSubmit=${submit}>
        <label class="field">
          <span>Folder name</span>
          <input
            class="input plain"
            type="text"
            value=${name}
            onInput=${(e) => setName(e.target.value)}
            placeholder="Holiday 2026"
            autocomplete="off"
          />
        </label>
        ${error ? html`<${Notice} error=${error} />` : null}
        <div class="actions">
          <button type="button" class="btn" onClick=${onClose}>Cancel</button>
          <button type="submit" class="btn primary" disabled=${busy || !name.trim()}>
            ${busy ? 'Creating…' : 'Create'}
          </button>
        </div>
      </form>
    <//>
  `;
}

export function RenameSheet({ machine, entry, link, capsStore = null, folderChanges = null, onRenamed, onClose }) {
  const [name, setName] = useState(entry?.name || baseFileName(entry?.path || ''));
  const [error, setError] = useState(null);
  const [busy, setBusy] = useState(false);

  async function submit(e) {
    e.preventDefault();
    const trimmed = name.trim();
    if (!trimmed || trimmed.includes('/') || trimmed.includes('\\')) {
      setError({ message: 'Enter a name without slashes.' });
      return;
    }
    const parent = parentFilePath(entry.path);
    const nextPath = joinFilePath(parent, trimmed);
    if (nextPath === entry.path) {
      onClose();
      return;
    }
    setBusy(true);
    setError(null);
    try {
      if (capsStore) {
        await capsStore.call(
          'fs.move',
          { from: entry.path, to: nextPath, overwrite: false },
          { on: machine, optional: ['overwrite'] },
        );
      } else {
        await link.call('fs.move', { from: entry.path, to: nextPath, overwrite: false }, { on: machine });
      }
      if (folderChanges) {
        folderChanges.changed(machine, parent || '~', { reason: 'rename' });
      }
      onClose();
      if (onRenamed) onRenamed(nextPath, trimmed);
    } catch (err) {
      setError(err);
      setBusy(false);
    }
  }

  return html`
    <${Sheet} title="Rename" onClose=${onClose}>
      <form onSubmit=${submit}>
        <label class="field">
          <span>New name</span>
          <input
            class="input plain"
            type="text"
            value=${name}
            onInput=${(e) => setName(e.target.value)}
            autocomplete="off"
          />
        </label>
        ${error ? html`<${Notice} error=${error} />` : null}
        <div class="actions">
          <button type="button" class="btn" onClick=${onClose}>Cancel</button>
          <button type="submit" class="btn primary" disabled=${busy || !name.trim()}>
            ${busy ? 'Renaming…' : 'Rename'}
          </button>
        </div>
      </form>
    <//>
  `;
}

// ── Search More filters sheet ────────────────────────────────────────────────────────────────

export function SearchFiltersSheet({ size, changed, onApply, onClose }) {
  const [nextSize, setNextSize] = useState(size || '');
  const [nextChanged, setNextChanged] = useState(changed || '');

  const sizeOptions = [
    { id: '', label: 'Any size' },
    { id: 'small', label: 'Under 1 MB' },
    { id: 'medium', label: '1 MB – 100 MB' },
    { id: 'large', label: 'Over 100 MB' },
  ];
  const changedOptions = [
    { id: '', label: 'Any time' },
    { id: 'day', label: 'Past 24 hours' },
    { id: 'week', label: 'Past week' },
    { id: 'month', label: 'Past month' },
    { id: 'year', label: 'Past year' },
  ];

  return html`
    <${Sheet} title="More filters" onClose=${onClose}>
      <p class="label">Size</p>
      <div class="chips" role="group" aria-label="File size">
        ${sizeOptions.map(
          (opt) => html`
            <button
              key=${opt.id || 'any'}
              type="button"
              class="chip-btn"
              aria-pressed=${nextSize === opt.id ? 'true' : 'false'}
              onClick=${() => setNextSize(opt.id)}
            >
              ${opt.label}
            </button>
          `,
        )}
      </div>

      <p class="label">Changed</p>
      <div class="chips" role="group" aria-label="Changed date">
        ${changedOptions.map(
          (opt) => html`
            <button
              key=${opt.id || 'any'}
              type="button"
              class="chip-btn"
              aria-pressed=${nextChanged === opt.id ? 'true' : 'false'}
              onClick=${() => setNextChanged(opt.id)}
            >
              ${opt.label}
            </button>
          `,
        )}
      </div>

      <div class="actions">
        <button
          type="button"
          class="btn"
          onClick=${() => {
            onApply({ size: '', changed: '' });
            onClose();
          }}
        >
          Clear
        </button>
        <button
          type="button"
          class="btn primary"
          onClick=${() => {
            onApply({ size: nextSize, changed: nextChanged });
            onClose();
          }}
        >
          Apply
        </button>
      </div>
    <//>
  `;
}

// ── Sync folder action sheets ────────────────────────────────────────────────────────────────

export function SyncConflictsSheet({ syncFolder, link, folderChanges = null, onChanged, onClose }) {
  const [conflicts, setConflicts] = useState(syncFolder?.conflicts || []);
  const [busyPath, setBusyPath] = useState('');
  const [error, setError] = useState(null);

  useEffect(() => {
    let alive = true;
    link
      .call('sync.conflicts', { id: syncFolder.id })
      .then((res) => {
        if (alive && Array.isArray(res?.conflicts)) setConflicts(res.conflicts);
      })
      .catch(() => {});
    return () => {
      alive = false;
    };
  }, [syncFolder.id]);

  async function resolve(c, keep) {
    setBusyPath(c.path);
    setError(null);
    try {
      await link.call('sync.resolve', { id: syncFolder.id, path: c.path, keep });
      if (folderChanges) {
        for (const mem of syncFolder?.members || []) {
          if (!mem?.machine || !mem?.path) continue;
          const full = joinFilePath(mem.path, c.path);
          folderChanges.changed(mem.machine, parentFilePath(full) || mem.path, { reason: 'sync-resolve' });
        }
      }
      setConflicts((prev) => prev.filter((item) => item.path !== c.path));
      if (onChanged) onChanged();
    } catch (err) {
      setError(err);
    } finally {
      setBusyPath('');
    }
  }

  return html`
    <${Sheet} title="Files with two versions" onClose=${onClose}>
      ${error ? html`<${Notice} error=${error} />` : null}
      ${conflicts.length === 0
        ? html`<div class="empty">No files have two versions right now.</div>`
        : html`
            <div class="list">
              ${conflicts.map(
                (c) => html`
                  <div class="row" key=${c.path}>
                    <div class="body">
                      <span class="title">${c.path}</span>
                      <span class="sub wrap">${c.summary || 'Both versions are kept.'}</span>
                      <div class="row-actions">
                        <button
                          type="button"
                          class="btn small primary"
                          disabled=${busyPath === c.path}
                          onClick=${() => resolve(c, 'newer')}
                        >
                          Keep newer
                        </button>
                        <button
                          type="button"
                          class="btn small"
                          disabled=${busyPath === c.path}
                          onClick=${() => resolve(c, 'both')}
                        >
                          Keep both
                        </button>
                      </div>
                    </div>
                  </div>
                `,
              )}
            </div>
          `}
    <//>
  `;
}

export function SyncBinSheet({ syncFolder, link, folderChanges = null, onChanged, onClose }) {
  const [entries, setEntries] = useState(syncFolder?.bin || []);
  const [busyPath, setBusyPath] = useState('');
  const [error, setError] = useState(null);

  useEffect(() => {
    let alive = true;
    link
      .call('sync.bin', { id: syncFolder.id })
      .then((res) => {
        if (alive && Array.isArray(res?.entries)) setEntries(res.entries);
      })
      .catch(() => {});
    return () => {
      alive = false;
    };
  }, [syncFolder.id]);

  async function restore(item) {
    setBusyPath(item.path);
    setError(null);
    try {
      await link.call('sync.restore', { id: syncFolder.id, path: item.path });
      if (folderChanges) {
        for (const mem of syncFolder?.members || []) {
          if (!mem?.machine || !mem?.path) continue;
          const full = joinFilePath(mem.path, item.path);
          folderChanges.changed(mem.machine, parentFilePath(full) || mem.path, { reason: 'sync-restore' });
        }
      }
      setEntries((prev) => prev.filter((e) => e.path !== item.path));
      if (onChanged) onChanged();
    } catch (err) {
      setError(err);
    } finally {
      setBusyPath('');
    }
  }

  return html`
    <${Sheet} title="Bin" onClose=${onClose}>
      <p class="sheet-sub">Deleted files stay here for 30 days so nothing is lost quietly.</p>
      ${error ? html`<${Notice} error=${error} />` : null}
      ${entries.length === 0
        ? html`<div class="empty">The bin is empty.</div>`
        : html`
            <div class="list">
              ${entries.map(
                (item) => html`
                  <div class="row" key=${item.path}>
                    <span class="body">
                      <span class="title">${item.path}</span>
                      <span class="sub">
                        ${formatBytes(item.size)} · deleted ${formatRelativeTime(item.deleted_at)}
                      </span>
                    </span>
                    <button
                      type="button"
                      class="btn small"
                      disabled=${busyPath === item.path}
                      onClick=${() => restore(item)}
                    >
                      ${busyPath === item.path ? 'Restoring…' : 'Restore'}
                    </button>
                  </div>
                `,
              )}
            </div>
          `}
    <//>
  `;
}

export function SyncAddMachineSheet({ syncFolder, machines, link, onChanged, onClose }) {
  const existing = new Set((syncFolder?.members || []).map((m) => m.machine));
  const candidates = (machines || []).filter((m) => !existing.has(m.name));
  const [machine, setMachine] = useState(candidates[0]?.name || '');
  const [path, setPath] = useState(`/home/me/${syncFolder?.name || 'Synced'}`);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState(null);

  async function submit(e) {
    e.preventDefault();
    if (!machine || !path.trim()) return;
    setBusy(true);
    setError(null);
    try {
      await link.call('sync.add', { id: syncFolder.id, machine, path: path.trim() });
      if (onChanged) onChanged();
      onClose();
    } catch (err) {
      setError(err);
      setBusy(false);
    }
  }

  return html`
    <${Sheet} title="Add a machine" onClose=${onClose}>
      ${candidates.length === 0
        ? html`<div class="empty">Every machine in your fleet is already in ${syncFolder?.name}.</div>`
        : html`
            <form onSubmit=${submit}>
              <label class="field">
                <span>Machine</span>
                <select value=${machine} onChange=${(e) => setMachine(e.target.value)}>
                  ${candidates.map((m) => html`<option key=${m.name} value=${m.name}>${m.name}</option>`)}
                </select>
              </label>
              <label class="field">
                <span>Folder path on ${machine}</span>
                <input
                  class="input"
                  type="text"
                  value=${path}
                  onInput=${(e) => setPath(e.target.value)}
                />
              </label>
              ${error ? html`<${Notice} error=${error} />` : null}
              <div class="actions">
                <button type="button" class="btn" onClick=${onClose}>Cancel</button>
                <button type="submit" class="btn primary" disabled=${busy}>
                  ${busy ? 'Adding…' : `Add ${machine}`}
                </button>
              </div>
            </form>
          `}
    <//>
  `;
}
