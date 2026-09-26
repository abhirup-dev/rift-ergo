# rift-ergo

Fast ergonomic workflows for the local Rift setup. `rift-ergo` is pinned to
Rift v0.5.10 and talks directly to Rift's Mach service through the typed
`rift-client` API. It does not shell out to `rift-cli`.

Rift 0.5.8.1 was previously tried and rolled back: it migrated windows between
displays on its own, with no commands issued. The later local installation used
revision `072411b` (just before v0.5.9); v0.5.10 adds display-scoped queries and
fixes focus-stealing during display churn and app-rule rediscovery. The earlier
uncommanded migration has not reproduced on v0.5.10 in daily use since
2026-09-23.

The helper is intentionally an orchestration layer, not a second window
manager. Window moves, display transfers, workspace activation, focus, state
queries, and events all use Rift's native primitives.

## Commands

| Command | Current binding | Behavior |
| --- | --- | --- |
| `rift-ergo move-follow <workspace>` | `Option+Shift+X` for each configured workspace key | Move the focused window to the workspace and its policy-selected display, then follow it. |
| `rift-ergo switch-workspace <workspace>` | `Option+X` for each configured workspace key | Activate the workspace on every display that holds windows of it; on the profile's display when it is empty everywhere. |
| `rift-ergo switch-workspace-both <workspace>` | `Option+Ctrl+X` for each configured workspace key | Activate the workspace on every display, returning focus to the display it started on. |
| `rift-ergo switch-workspace-focused <workspace>` | — (Raycast extension) | Activate the workspace on the focused display only. |
| `rift-ergo move-window-to-display <next\|prev>` | `Option+Shift+\`` (`next`) | Move the focused window to the workspace currently active on the adjacent display, then follow it. |
| `rift-ergo move-workspace-to-display <next\|prev>` | `Option+Shift+Tab` (`next`) | Move every window in the focused workspace to the adjacent display while preserving workspace membership and restoring focus. |
| `rift-ergo on-focus-change <event-json>` | — (Rift `subscribe cli` hook) | Show a focused window's workspace when Rift's own auto-switch dropped it. |
| `rift-ergo on-windows-changed [--init\|<event-json>]` | — (Rift `subscribe cli` hook) | Place a newly created window on its configured display, once, the first time it is seen. |
| `rift-ergo rehome [workspace]` | `Option+Shift+0` (focused workspace) | Move policy-matched windows from one selected workspace to their configured workspace and display homes. |

Workspace arguments are Rift workspace names, including numeric names such as
`1` and alphanumeric names such as `A` or `W`.

### `move-follow`

On the same display, this uses Rift's native
`MoveWindowToWorkspace { follow: true }`. Across displays, it activates the
target workspace on the configured display, transfers the exact window, uses
the same native follow command, and verifies the final window identity and
location.

### `switch-workspace`

`Option+X` is adaptive. Displays whose copy of workspace `X` holds windows all
switch to it: both when the workspace is spread across both, one when it lives
on one. A workspace that is empty everywhere opens on the display the profile
assigns it. The display the keyboard is on is switched last, so focus stays
there when it is among the targets and otherwise follows to the display that
was switched. `switch-workspace-both` and `switch-workspace-focused` keep
their explicit scopes.

### Display moves

`move-window-to-display` targets whichever workspace is already active on the
other display. `move-workspace-to-display` targets the same named workspace on
the other display.

`move-workspace-to-display` hands the relocation to Rift's own
`MoveWorkspaceToDisplay`, which moves the layout tree of the source display's
active workspace into the destination's workspace at the same ordinal, shows it
there, and raises the windows. Measured over 44 moves: a median of about 900 ms
and 43 clean, against 3832 ms for the per-window transfer loop it replaced. The
destination's other workspaces are untouched -- only the windows of the source
workspace move, and the workspace that was showing on the destination simply
stops showing.

What remains around that one command is everything it does not do:

- **Choosing the source.** The command carries no source; Rift reads it from
  whichever display is active when the command runs. So the source has to be
  bound first, and it has to be bound *after* every step that moves focus.
- **Showing the source workspace.** Rift moves the workspace that is on screen,
  not the one holding the key window. Focusing a window does not show its
  workspace, so that is an explicit step.
- **Escaping fullscreen on both displays.** Source and destination are both
  filtered by "is this space active", so a native-fullscreen space on either end
  makes the command a silent no-op.
- **Noticing that nothing happened.** Rift declines the command on eight
  separate paths -- a drag in progress, either space unavailable, source and
  destination resolving to the same space, an empty workspace, a layout engine
  with nothing to relocate -- and every one of them logs to Rift's own log and
  returns a successful IPC send. Only the state check separates a move from a
  no-op.
- **Restoring focus.** Rift picks the destination's focus by its own preference,
  which is not necessarily the window that had it.
- **Finishing a partial move.** Once in 44 moves the command left a window
  behind. A bounded repair pass moves the stragglers individually, using the
  same confirmed transfer that `move-window-to-display` uses. It sits inside the
  move step's timeout branch, so a successful move never reaches it: over 20
  controlled moves it engaged zero times. The cost is paid only when the
  alternative is a workspace split across two displays.

`RIFT_ERGO_NATIVE_WORKSPACE_MOVE=off` falls back to the previous
transfer-and-reconcile implementation, kept in `move_workspace_legacy.rs` until
the native path has enough live use behind it.

### Which workspace a command starts from

The source workspace follows the **key window**, not the display macOS reports
as active, and the command refuses when the two cannot be reconciled. The two disagree often enough to matter: focus and the active-menu-bar
display are updated by separate asynchronous paths, and a workspace move issued
in the gap between them runs on the wrong display. Two of three early
`move-workspace-to-display` runs went the wrong way for exactly this reason. A
three-way probe showed the APIs themselves agree 24 out of 24 times within
150 ms, so the disagreement is not a stale read -- something moved focus between
the check and the command. Reading the key window instead makes the source the
window the user is actually looking at.

Focus is also not durable. Alternating hard focus across displays twelve times,
all three views agreed 24 out of 24 at both 250 ms and 1.25 s -- but on two of
the six moves to the builtin, focus had been taken back by another application
within 1.25 s. Within a single display, 24 of 24 held. So the source is re-read
immediately before the command is sent, not once at the start.

There is one state that neither signal resolves: the key window in a workspace
that is not the one on screen. `on-focus-change` normally closes that gap by
showing the key window's workspace, but when it persists the two answers name
different workspaces and nothing can tell which the keypress meant. The move
refuses and says so, rather than relocating the wrong one.

### How Rift's commands are scoped

Two facts about Rift shape every workflow here:

- Layout commands (`SwitchToWorkspace`, `MoveWindowToWorkspace`) act on the
  display macOS reports as active, which Rift reads from
  `SLSCopyActiveMenuBarDisplayIdentifier`. A `MoveWindowToWorkspace` for a
  window on any other display is silently ignored.
- `MoveWindowToDisplay` drops the window into whatever workspace the
  destination is showing, not the one with the same name, and it raises the
  window there.

So before a workspace can be switched or windows re-homed on a display, that
display must be made active, and macOS only does that for a display that
gains the key window. `placement::acquire_display` tries, in order: focus a
window visible in the display's active workspace; focus a window Rift keeps
hidden in the target workspace there; click the desktop, which works on the
primary display only; focus any hidden window there and wait for Rift to
bring its workspace forward. `move-workspace-to-display` has one more option
when all of those fail: move the window that will end up focused first and
focus it there. Activating Finder and `SLSSetActiveMenuBarDisplayIdentifier`
were both tried and do nothing.

Physical placement is confirmed with `CGSCopySpacesForWindows`, the
WindowServer's own answer for which space a window is on, rather than a frame
heuristic. Rift moves workspace membership before the window itself, and a
window that is a member of the destination workspace while still sitting on
the source display is exactly what Rift later "corrects" by pulling the
membership back.

### `on-focus-change`

Rift auto-switches to a window's workspace only while handling an app
activation, and only for the window the app reports as main at that moment. An
app that moves focus to a window Rift does not manage right after activating --
Zoom focuses an untitled helper window as it comes forward -- replaces that main
window before Rift reads it, so the switch is dropped. The window keeps focus
while parked offscreen in a hidden workspace, and because the app is now
frontmost, activating it again changes nothing. Third-party switchers that raise
a specific window, such as BetterCmdTab, hit this every time; measured 4 out of 4
with Zoom.

Rift publishes the focus event either way. `on-focus-change` is registered
through `rift-cli subscribe cli` by
`~/.config/rift/scripts/install-focus-follow`, runs once per focus change, and
switches only when the focused window is one Rift manages in a workspace that is
not showing. Everything else exits in about a millisecond without contacting
Rift: an untracked window belongs to no workspace, and a window in the active
workspace needs nothing. Acting costs about 20 ms.

`RIFT_ERGO_FOCUS_FOLLOW=off` disables it; `=dry-run` logs the switch it would
make without making it.

#### The focus lease

`placement::acquire_display` deliberately focuses windows Rift keeps hidden, so
the workflows here generate exactly the events this hook exists to answer.
`src/focus_lease.rs` is the shared signal: a timestamp meaning "ignore focus
events until". Every confirmed transaction step pushes it forward, so it covers
a workflow for as long as that workflow is driving Rift and expires on its own
if the process dies; a longer grace after the last step covers the events Rift
publishes while the result settles.

Measured over nine cross-display `switch-workspace-both` runs: the hook would
have fired 8 and 10 times in two unleased runs, and fired 0 times with the lease,
with no change in workflow failures.

### `on-windows-changed`

Rift's own `app_rules` already assign a workspace to a new window, but they
resolve it inside the space the window was born in. A rule therefore means
"workspace 7 of whichever display macOS happened to open this app on"; no
version of Rift can express "workspace 7 of the builtin", and the maintainer has
said multi-display policy will stay second class for some time, reachable
through the CLI and IPC hooks. That gap is the whole reason this hook exists.
It fixes the display first, then re-asserts the workspace, because a display
transfer drops a window into whatever workspace the destination is showing.

It is deliberately not a reconciler over all windows. Each window is placed once,
the first time it is seen, so moving a window yourself afterwards sticks. A
window that fails to land stays marked as seen rather than being retried on every
later event -- retrying is how the script this replaces fought the user for
placement.

State lives in three files: `/tmp/rift-new-window-seen.json` (the placed set),
`/tmp/rift-new-window-ready` (the pid of the Rift the baseline belongs to), and
`/tmp/rift-new-window.lock` (single flight). `~/.config/rift/scripts/install-new-window-reconciler`
runs `on-windows-changed --init` at startup to record every existing window as
already placed, so a Rift restart re-derives nothing; events arriving before that
baseline lands are ignored, because they would treat the whole desktop as new.

Readiness is checked against the pid launchd reports for `sh.brew.rift`. Two
earlier attempts were both silently wrong: `pgrep -x rift`, and even a bare
`pgrep rift`, return nothing in the environment Rift spawns hooks in -- the
Python reconciler this replaces used `pgrep` for the same check, so its guard
never passed and it never placed a window. Matching a process *name* instead only
moved the problem, because the binary is not always called `rift` and a renamed
build disabled the hook again without a word. The launchd label is the one
identifier that does not change when the binary does.

`RIFT_ERGO_NEW_WINDOW=off` disables it; `=dry-run` logs the placements it would
make. `RIFT_ERGO_NEW_WINDOW_STATE` points the placed set elsewhere, which is how
a dry run is compared against the implementation currently in charge without
stealing its state.

### Notifications

Keybindings run through Rift's `exec`, which discards stderr, so a workflow that
fails -- or that refuses to act because the state was ambiguous -- is invisible
at the moment it matters. The log records everything, but nobody reads a log
after pressing a hotkey. A notification is fired after the work completes and
never waited on, so it costs the workflow nothing.

`RIFT_ERGO_NOTIFY` selects what is worth interrupting for:

| Value | Notifies on |
| --- | --- |
| `off` | nothing |
| `errors` | failures and refusals |
| `moves` | the default: failures, refusals, and commands that relocated windows |
| `all` | every invocation |

Workspace switches are silent under the default because they are self-evident on
screen and fire constantly. A workflow that moved something records what it did
through `notify::summarize`, so the body says "moved 3 windows to Built-in"
rather than repeating the command line. Delivery is `terminal-notifier`, with
`osascript` as the fallback if that Homebrew package disappears.

### `rehome`

With no argument, `rehome` snapshots only the currently focused workspace.
With an explicit argument, such as `rehome W`, it first activates `W` on its
policy-selected display and snapshots that workspace. It never scans or
reconciles every workspace in the system.

The workflow then:

1. Matches the captured windows against the selected policy profile.
2. Groups matches by configured workspace and display.
3. Skips the transaction entirely when every matched window is already home.
4. Uses Rift's native typed display-transfer and
   `MoveWindowToWorkspace { follow: false }` commands for misplaced windows.
5. Restores the selected source workspace and verifies the final groups once.

Unmatched windows are intentionally left alone. Policy parsing, profile
selection, alias resolution, and rule resolution happen once per invocation.

## Routing policy

The machine-specific policy is deliberately kept outside this repository:

```text
~/.config/rift/reconcile/workspace-assignments.json
```

Override it with `RIFT_ERGO_POLICY`:

```sh
RIFT_ERGO_POLICY=/path/to/workspace-assignments.json \
  rift-ergo rehome 7
```

Rift does not natively parse this JSON file. The live Rift configuration passes
its path to `rift-ergo` in the `Option+Shift+0` binding. Rift's native
`virtual_workspaces.app_rules` remain responsible for launch-time workspace
routing; the external policy adds monitor-profile selection and the explicit
rehome operation for existing windows.

Profiles select connected displays through aliases, define workspace-to-display
defaults, and contain ordered window rules. A rule may match bundle ID, app
name, or title substring; the first matching rule wins. Each rule supplies its
home workspace and may override its home display.

The current policy is small, so rules are resolved with one linear pass over
the captured workspace. Precompiling a separate index would add complexity
without improving the IPC-bound part of the workflow.

## Project structure

```text
rift-ergo/
├── Cargo.toml                 Rust package and pinned rift-client revision
├── Cargo.lock                 Reproducible dependency graph
├── Makefile                   Check, release build, and atomic install
└── src/
    ├── main.rs                CLI parsing and command dispatch
    ├── policy.rs              Monitor profiles and window-home matching
    ├── focus_lease.rs         Suppression shared with the focus-follow hook
    ├── notify.rs               Desktop notifications for keybinding-run commands
    ├── rift.rs                Typed Rift queries and command constructors
    ├── transaction.rs         Event/state confirmation with bounded deadlines
    └── workflow/
        ├── mod.rs
        ├── placement.rs       Shared target preparation and follow mechanics
        ├── move_follow.rs
        ├── move_window_to_display.rs
        ├── focus_follow.rs
        ├── new_window.rs
        ├── move_workspace_to_display.rs
        ├── move_workspace_legacy.rs
        └── rehome.rs
```

Crossbeam is limited to one bounded event channel and deadline-based receives.
There is no async runtime, general workflow framework, background daemon, cron
job, or polling loop.

Batch display moves use an adaptive transaction budget with a 30-second
minimum: `max(30 seconds, 2.5 seconds + 2 seconds per source window)`. A
synchronous Rift query already in flight cannot be preempted, so this is a
cooperative rather than a hard wall-clock deadline.

Physical placement checks are display-topology and layout aware. Workspace
membership is the conservative fallback when scrolling geometry cannot safely
distinguish a display.

## Logging

Keybindings run `rift-ergo` through Rift's `exec`, so stderr is discarded and a
failing binding is otherwise silent. Every invocation appends to
`~/Library/Logs/rift/rift-ergo.log`. Failures are
tagged `source=rift` or `source=rift-ergo` so a Rift regression is not mistaken
for one here. `rift-ergo --log-path` prints the path; `RIFT_ERGO_LOG=off`
disables it.

## Development and installation

```sh
make check
make install
```

The default installation path is
`~/.config/rift/bin/rift-ergo`. `make install` builds with the locked dependency
graph and atomically replaces the executable. Override the destination with:

```sh
make install PREFIX=/another/path
```

The install target does not copy or modify the machine-specific routing policy.
