# Machines: what a machine is doing, and waking it on the way

A machine's screen answers four questions without a terminal: is it on, is it busy, with what,
and is anything wrong. And when something a person starts needs a machine that is off, Legion
offers to wake it and carries on by itself.

Everything here is tools first: the screen shows what `sys.info` and `sys.procs` return and stops
programs with `sys.kill`, so an agent sees and can do the same.

## What the node reports

### `sys.info` gains

Every field below is optional. Absent means "this machine does not report it". A value is never
guessed and never zero as a stand-in.

- `cpu.temperature_c`: the processor's package temperature.
- `gpus[]` gains `vendor`, `temperature_c` and `power_w`. Graphics cards that are not NVIDIA are
  listed too where the system exposes them (on Linux through `/sys/class/drm`), with whichever of
  `utilization_pct`, `memory_total` and `memory_used` the driver gives.
- `battery`: `{percent, state, on_ac, time_left_s}` with `state` one of `charging`,
  `discharging`, `full`, `unknown`. Absent on a machine without a battery.

Where they come from: Linux reads `/sys/class/hwmon`, `/sys/class/power_supply` and
`/sys/class/drm`, and asks `nvidia-smi` when it exists. macOS and Windows report the battery
through the system's own interface and NVIDIA cards through `nvidia-smi`; temperatures there are
absent rather than read through private interfaces.

Each source is a separate probe with its own one-second limit. A probe that fails or is slow
leaves its fields out; it never fails the call and never delays it past that limit. Probes run
side by side, so `sys.info` stays as fast as its slowest single probe.

### `sys.procs` gains

- `gpu_memory` on a process, when the graphics driver reports memory per process.
- `sort: "gpu_memory"`.
- `self: true` on the node's own process and its ancestors, which no screen offers to stop.
- `group: true`: one row per program instead of one per process. Processes belong together when
  they have the same name and the same user. A row is `{name, user, count, cpu_pct, memory,
  gpu_memory, members, more}`: the figures are sums, `members` is up to 64 `{pid, start_time}`
  pairs, busiest first, and `more` counts the members left out. `limit` and `sort` apply to rows.

`cpu_pct` keeps its meaning (100 is one whole core). The share of the whole machine is
`cpu_pct / cpu.cores`, and that is what a person is shown.

### `sys.kill` gains

- `targets`: 1 to 64 `{pid, start_time}` pairs as an alternative to `pid`. `start_time` is
  required here, so a process id that was reused in the meantime is never stopped by mistake.
- The result lists each target as `stopped`, `gone` (already ended), `reused` (another process
  has that id now; nothing done), `still_running`, or `refused` with a `code` and a sentence.
- Without `force` the tool asks the processes to exit and waits up to five seconds. What is left
  is `still_running`, and the caller decides whether to call again with `force`.
  - On Linux and macOS, asking sends `SIGTERM` and `force` sends `SIGKILL`.
  - On Windows, asking posts `WM_CLOSE` to the process's top-level windows and sends
    `CTRL_BREAK_EVENT` where its console process group allows it; if neither applies (a
    windowless process outside an accessible console group), it reports `still_running` right away
    with a sentence saying it must be forced. `TerminateProcess` is used only with `force`.
- Refused always: process id 0 and 1, the node's own process and its ancestors, and non-leader
  thread ids on Linux. A process of another user is whatever the operating system answers,
  reported per target.

The effect stays `destroy`, so who may stop programs is what the role already says.

## The screen

Overview, top to bottom. Nothing is shown for a value the machine does not report: no empty
meters, no dashes.

1. **State.** One line: "On for 3 days" with "Busy" when the processor or a graphics card is
   above 60 percent, otherwise "Idle". The existing lines for off, asleep and waking stay.
2. **Meters.** Processor, Memory, Graphics (use and memory; one row per card when there are
   several), Storage (the fullest real disk, as free space), Temperature (the hottest reported
   part), Battery. A meter turns amber when it needs attention: storage under 5 percent free,
   85 °C or more, battery at 15 percent or less while not charging.
3. **Using it now.** Up to five programs from `sys.procs {group: true}`, those using at least
   5 percent of the whole processor, 5 percent of memory, or 256 MB of graphics memory. Each row
   is the name, "× 12" when it is several processes, and its largest figure in words: "62% of the
   processor", "9.1 GB of graphics memory". When nothing qualifies: "Nothing much is running."
   Tapping a row opens a sheet with all its figures, since when it runs and as whom; with
   Technical details also the command line and process ids.
   - **Stop** asks first: "Stop rustc on tower? Anything unsaved in it is lost." It sends the
     row's members as `targets`. If some are `still_running`, the sheet says so and offers
     **Force stop**. Refusals are shown as the sentence the node gave.
   - A device that may not stop programs sees the sentence that says so instead of the button,
     as everywhere else.
4. **Running** (jobs started through Legion) and the details list stay as they are.

Refreshing: while Overview is visible and the app is in front, `sys.info` and
`sys.procs {group: true, limit: 12}` every five seconds. A tick is skipped while the previous
answer is outstanding, so calls never pile up on a slow connection. Values older than fifteen
seconds are dimmed. Hidden screens ask nothing.

On the Machines list a machine gets one amber word when a meter above is amber ("Storage almost
full", "Running hot", "Battery low"), from the `sys.info` the list already has. No extra calls.

## Wake it and continue

When a person starts something that needs a machine that does not answer, and that machine can
be woken, and this device may use `power.wake`, they get one sheet instead of an error:

> **tower is off.**
> Wake it and continue · Not now

On yes, Legion sends the wake through a machine that is on, then asks `fleet.ping` every three
seconds for up to three minutes, showing "Waking tower… 0:24". When the machine answers, the
thing the person started happens, exactly once.

- What counts as started: opening the machine's screen or its files, sending or moving a file to
  or from it, running an action or a command on it.
- If the person leaves the screen while waiting, opening a screen is forgotten. A transfer, an
  action or a command they started still runs when the machine answers, and reports in Activity.
- If it does not answer in time: "tower did not answer after 3 minutes. It may need longer to
  start, or waking it may not be working." with Try again.
- Two things waiting for the same machine share one wake and one sheet.
- Nothing wakes a machine without a person pressing the button for that occasion. A device that
  may not wake machines, or a machine without wake set up, keeps today's plain sentence.

In the interface this is one helper, `ensureAwake(machine)`, that every such starting point
goes through, and one shared sheet.

Agents need nothing new: `power.wake` with `wait_s` already sends and waits. The handbook's
agent page gets the three-line recipe.

## Later: a day of history

A small line per meter showing the last day needs samples taken while nobody is looking, and a
node is not always running. That will be a `sys.history` tool fed by a once-a-minute sampler
(inside `legion serve` where that runs, otherwise a timer the installer sets up), kept in a
fixed-size file. It is its own piece of work because it changes what the installer puts on a
machine; this document does not include it.

## Tests

- Each probe against fixtures: a `hwmon` tree, a `power_supply` tree, a `drm` tree, recorded
  `nvidia-smi` output, including missing, malformed and slow sources.
- Grouping: sums, ordering, the 64-member cap and `more`.
- `sys.kill` with `targets`: stopped, already gone, a reused id, one that ignores the request
  until forced, and each refusal.
- Interface: meters absent when values are absent; amber thresholds; the stop and force-stop
  flow; refresh never overlapping; wake and continue with a clock under test control, including
  timeout, leaving the screen, and two waiters sharing one wake.
