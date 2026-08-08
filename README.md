# rift-ergo

Small typed-IPC helpers for the local Rift configuration. The project is pinned
to Rift v0.5.3's exact revision and communicates directly with Rift's Mach
service through `rift-client`; it does not shell out to `rift-cli`.

## Commands

```sh
rift-ergo move-follow W
```

`move-follow` resolves the configured monitor profile and runs one of two
workflows:

- Same display: use Rift's native workspace move with `follow = true`.
- Cross display: focus the destination, activate its workspace, transfer the
  exact window, apply Rift's native workspace-follow command, then focus and
  verify the exact window.

The cross-display workflow is an explicit state machine in `src/workflow.rs`.
Transport and timeout mechanics do not appear in that state machine.

## Structure

- `main.rs`: CLI and top-level error reporting.
- `policy.rs`: JSON policy loading and display-profile resolution.
- `rift.rs`: typed Rift state queries and command construction.
- `transaction.rs`: command/event/state synchronization with one deadline.
- `workflow.rs`: the visible same-display and cross-display behavior.

Crossbeam is intentionally limited to one bounded event channel and
`recv_deadline`. There is no async runtime or general workflow framework;
retries remain local and bounded inside the batch state machine.

Batch display moves use an adaptive transaction budget with a 30-second
minimum: `max(30 seconds, 2.5 seconds + 2 seconds per source window)`. The
per-window component covers an initial transfer and one targeted recovery
allowance. Each individual window transfer wait is nominally capped at 1
second, while setup, reconciliation, and final settling retain their normal
per-phase allowance within the total budget. A synchronous Rift query already
in flight cannot be preempted, so this is a cooperative rather than a hard
wall-clock deadline.

Physical placement checks are topology- and layout-aware. With vertically
separated displays, the direct window frame's y-axis identifies its display
while scrolling columns may move arbitrarily far off-screen on x. When
scrolling displays cannot be distinguished on a safe axis, workspace
membership remains the conservative fallback.

Single-window workspaces take an optimistic fast path. If the same workspace is
already active on the destination, only workspace activation is skipped; the
destination display itself is always focused before transfer. The typed display
transfer uses a short first attempt and one idempotent retry, then waits for the
exact window's direct frame to enter the target display before issuing
exact-window focus. Singletons skip group reconciliation, ordering work, and
the final batch quiescence barrier. A timeout refreshes live display state
before falling back to the full correctness workflow.

## Deferred: layout-order restoration

`move-workspace-to-display` currently reconciles window membership and display
placement, and restores the originally focused window when Rift reports a
stable focus transition. It intentionally does not reconstruct the source
layout order.

If ordering becomes important later, add it as a post-transfer correctness
layer:

1. Before moving, capture a layout-specific order from window frames. Do not
   use `WorkspaceData.windows`; Rift exposes that in membership order rather
   than visual layout order.
2. Complete the existing transfer, reconciliation, and settling workflow.
3. Derive the permutation between the captured order and the destination
   slots.
4. Restore it with Rift's typed `SwapWindows` primitive using cycle
   decomposition, which needs at most `n - 1` swaps.
5. Verify the final order and restore the original focused window.

Implement adapters independently for each supported layout. Stack can use its
linear frame offsets; scrolling can group frames into columns and then rows.
Floating windows should be excluded from tiled ordering. Traditional/BSP
topology restoration should remain deferred until Rift exposes enough
structure to reproduce it accurately.

This layer improves layout correctness but does not reduce IPC traffic: the
transfer still needs one command per window, and order restoration adds zero
to `n - 1` swap commands plus verification. Keep it optional unless preserving
visual order becomes worth that cost.

## Development

```sh
make check
make install
```

The default installation path is `~/.config/rift/bin/rift-ergo`. Override it
with `make install PREFIX=/another/path`.
