# rift-ergo

Fast ergonomic workflows for the local Rift setup. `rift-ergo` is pinned to
the installed Rift v0.5.3 revision and talks directly to Rift's Mach service
through the typed `rift-client` API. It does not shell out to `rift-cli`.

The helper is intentionally an orchestration layer, not a second window
manager. Window moves, display transfers, workspace activation, focus, state
queries, and events all use Rift's native primitives.

## Commands

| Command | Current binding | Behavior |
| --- | --- | --- |
| `rift-ergo move-follow <workspace>` | `Option+Shift+X` for each configured workspace key | Move the focused window to the workspace and its policy-selected display, then follow it. |
| `rift-ergo move-window-to-display <next\|prev>` | `Option+Shift+\`` (`next`) | Move the focused window to the workspace currently active on the adjacent display, then follow it. |
| `rift-ergo move-workspace-to-display <next\|prev>` | `Option+Shift+Tab` (`next`) | Move every window in the focused workspace to the adjacent display while preserving workspace membership and restoring focus. |
| `rift-ergo rehome [workspace]` | `Option+Shift+0` (focused workspace) | Move policy-matched windows from one selected workspace to their configured workspace and display homes. |

Workspace arguments are Rift workspace names, including numeric names such as
`1` and alphanumeric names such as `A` or `W`.

### `move-follow`

On the same display, this uses Rift's native
`MoveWindowToWorkspace { follow: true }`. Across displays, it activates the
target workspace on the configured display, transfers the exact window, uses
the same native follow command, and verifies the final window identity and
location.

### Display moves

`move-window-to-display` targets whichever workspace is already active on the
other display. `move-workspace-to-display` targets the same named workspace on
the other display.

A one-window workspace uses a short optimistic fast path. Multi-window moves
use the batch workflow, restore the originally focused window, and perform
bounded reconciliation only if Rift's resulting state is incomplete.

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
    ├── rift.rs                Typed Rift queries and command constructors
    ├── transaction.rs         Event/state confirmation with bounded deadlines
    └── workflow/
        ├── mod.rs
        ├── placement.rs       Shared target preparation and follow mechanics
        ├── move_follow.rs
        ├── move_window_to_display.rs
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

## Deferred: visual layout order

`move-workspace-to-display` preserves membership and focus but does not
reconstruct the source layout's exact visual order. If this becomes important,
it can be added after transfer as a layout-specific correctness layer using
Rift's typed `SwapWindows` primitive. That would add up to `n - 1` IPC commands,
so it remains deferred until the visual-order benefit justifies the extra work.

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
