# rift-ergo

Fast ergonomic workflows for the local Rift setup. `rift-ergo` is pinned to
the installed Rift v0.5.5 revision and talks directly to Rift's Mach service
through the typed `rift-client` API. It does not shell out to `rift-cli`.

Rift 0.5.8.1 was tried and rolled back: it migrated windows between displays
on its own, with no commands issued. Its client crate differs from 0.5.5 only
by two additive protocol fields, so upgrading later is a small change here;
the reason to wait is the daemon's behaviour, not the API.

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

`move-workspace-to-display` first shows the target workspace on the
destination, then transfers the windows one at a time, each confirmed on the
destination by the WindowServer before the next, restores focus to the window
that had it, and verifies once the state is stable. Transfers are sequential
because Rift drops a queued transfer of the focused window, and the focused
window goes last so Rift's raise-on-transfer leaves focus where the workflow
wants it. Bounded reconciliation runs only if a transfer landed elsewhere.

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
    ├── rift.rs                Typed Rift queries and command constructors
    ├── transaction.rs         Event/state confirmation with bounded deadlines
    └── workflow/
        ├── mod.rs
        ├── placement.rs       Shared target preparation and follow mechanics
        ├── move_follow.rs
        ├── move_window_to_display.rs
        ├── focus_follow.rs
        ├── move_workspace_to_display.rs
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
`~/Library/Logs/rift/rift-ergo.log`, next to the reconciler's log. Failures are
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
