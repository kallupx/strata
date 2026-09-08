# Issue 598: local evidence and replay

Base: `110a433fd8c4e0787c3652550809478ef573b643`, freshly fetched from `lgse/strata` upstream/main. Worktree: `<worktree>`; branch: `fix/598-drag-cancel-animation`.

`evidence/before/` was captured before editing production code. `before-binary.sha256` identifies that saved executable, also retained as `before/strata` (the harness requires the executable name `strata`). `evidence/after/` uses the patched binary. The later `*-recording/` directories replay those same binaries and additionally retain every raw frame for MP4 encoding. The initial captures are retained unchanged.

All captures use the verified published canonical Ubuntu container: GTK 4.14.5, Rust 1.98.1, Xvfb 1440×900×24 at 96 DPI, a 1200×760 Strata window, scale 1, cairo renderer, Adwaita GTK theme, Strata azure-glow theme, and the harness's pinned fonts/preferences. GTK animations are explicitly enabled; Reduce motion is false except for the `reduced` case. Folder peeking is disabled to separate #630. Each run owns a private display, session/accessibility buses, HOME/XDG directories, and disposable fixture at `/tmp/strata-598-capture` inside its own container.

Fixture: `a.txt` through `h.txt`, each containing `<letter>: fixture 598\n`, and empty `dest/`. `failed` sets `dest/` to mode 0555 before launching. Each scenario verifies source/destination existence and exact text, then restores permissions and removes only its own fixture. Listings and window dimensions are in each `results.json`.

## Replay

From the worktree:

```bash
# Persistent rootless tools/config/store, isolated from other sessions:
source target/issue-598/env.sh
# Verify/reuse the pinned base; never rebuild it for application edits.
python3 scripts/e2e_base.py ensure

# Build the current app in this task's container build directory:
target/issue-598/container.sh cargo build --locked --bin strata

# Use NEW output names to preserve the delivered evidence:
target/issue-598/container.sh env \
  STRATA_BINARY=/workspace/target/issue-598/before/strata \
  /opt/e2e-venv/bin/python target/issue-598/capture.py before-replay

target/issue-598/container.sh /opt/e2e-venv/bin/python \
  target/issue-598/capture.py after-replay
```

`capture.py` starts a real XTEST drag from accessible icon bounds, holds it, then releases or sends Escape. Copy holds Control through the drop. No-op drops on the current pane background; failed drops on the read-only destination. Input never reaches the user's display.

The capture loop samples the full isolated root window at nominal 60 Hz for 650 ms after release, recording actual monotonic capture times. Named screenshots select frames nearest 83, 150, 217, and 333 ms. MP4s encode `001.png` onward at 60 fps; their first frame is immediately after release. The extra raw `000.png` is the held drag. GIFs include a short held frame and have centisecond timing quantization; use MP4s and timestamps for timing. The initial GIFs use the original recorder's approximate playback timing.

```bash
ffmpeg -framerate 60 -start_number 1 \
  -i target/issue-598/evidence/after-replay/escape-frames/%03d.png \
  -c:v libx264 -crf 18 -pix_fmt yuv420p \
  target/issue-598/evidence/after-replay/escape.mp4
```

During cancellation, GTK also animates the drag icon returning toward the pointer/source. That is distinct from the source-row exit animation tested here. Escape can change selection color. The regression measures glyph position and opacity recovery rather than requiring whole-window pixel identity.

## Trace and scope

- `src/ui/browser/columns/rows.rs`: `prepare` provides the selected files and widget paintable; `drag-begin` dims the row; `drag-end` previously unconditionally ran `slide_out`. Folder drops return `true` immediately and schedule `start_transfer` 300 ms later. The patch retains drag cleanup and destination `slide_in_down`.
- `src/ui/modal.rs` and `src/style.css`: `slide_out` applies `slide-out-up` for 240 ms, translating up 14 px and fading to zero. Its other caller is Quick Preview (`src/ui/preview.rs`); that separate behavior and helper are unchanged.
- `src/ui/browser/clipboard.rs`: current-pane drops route to the same `ViewState::start_transfer`; modifier/action selection distinguishes local moves and copies. List/Grid already only remove the dragging class at drag end.
- `src/ui/browser/transfer.rs`: `start_transfer` rejects no-ops and unsupported destinations, resolves collisions, then calls the browser operation provider. Acceptance is not completion.
- `src/adapters/local_operations.rs`: `paste` spawns an async task, awaits size discovery and GIO copy/move operations, accumulates completed items, and emits `Pasted`, `TransferFailed`, or cancellation results. Its lower-level no-op path also counts an item as completed; the UI normally filters those requests first.
- `src/app/browser.rs`: the operation listener uses the provider completion list for `TransferFinished.moved_locations` when moving (including partial failure/cancellation). Copies and failures without completed items report none. Successful operations emit `TransferReveal` and `TransferCompleted`; file monitors update the source listing.
- `src/ui/browser/events.rs`: successful transfer reveal opens/reloads the destination and selects the transferred entries. This existing feedback is retained, avoiding new drag tracking or a speculative timer for source removal.

GTK documents `delete_data` as a request to delete source data for MOVE, not confirmation that Strata's filesystem operation has completed: https://docs.gtk.org/gtk4/signal.DragSource.drag-end.html. `DropTarget::drop` returning true means acceptance: https://docs.gtk.org/gtk4/signal.DropTarget.drop.html. Consequently gating the old animation only on `delete_data` would still misrepresent no-op, conflicted, and failed moves.

#630 remains a separate peek fix. Its row patch fails a read-only application check at the shared drag-end hunk (`pr-630-overlap.log`). When combining, keep its begin/end `cancel_peek()` calls and this patch's removal of `slide_out`; no #630 code was cherry-picked. #337 requested a different animation replacement and is out of scope.


## Presentation update: transparent popups and window framing

The preferred evidence is now `evidence/before-composited/` and
`evidence/after-composited/`. These are new captures from the same saved upstream
and patched executables, with xcompmgr 1.1.10 (`-n`, no added shadows/fades)
compositing the private Xvfb display. This renders GTK popup/drag-surface alpha
correctly instead of showing black rectangles. Captures frame the actual
1200×760 application window, excluding unused black space around it.
No application pixels are painted out or synthesized. Original evidence remains
available in its original directories. All seven filesystem assertions are
repeated during both captures; GTK, theme, fixture, scale, and window dimensions
match between the new BEFORE and AFTER sets.

Replay using `capture-composited.py` in place of `capture.py` in the commands
above, with fresh output directory names. The task-local compositor executable
is `target/issue-598/tools/usr/bin/xcompmgr`; it runs inside the existing pinned
container against its system libraries and only the harness-owned display.
The canonical E2E base and application source are unchanged. Use the same
60 fps FFmpeg encoding command for the new raw frame directories.
