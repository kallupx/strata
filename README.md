# Evidence for lgse/strata#598 (PR #637)

Matched captures of the saved upstream binary and the drag-animation fix. All media use a private compositor and frame the 1200×760 application window; no pixels were retouched.

- **Before:** upstream `110a433fd8c4e0787c3652550809478ef573b643`.
- **After:** the production drag fix in `818f9f8`, also present in PR head `7e2214b8a4352032be7a7f63065f17c43555dce6`.
- **Cases:** Escape, outside release, copy, move, no-op, failed transfer, reduced-motion cancellation.
- MP4 recordings: 60 fps; GIF previews have centisecond timing quantization.
- Screenshots at 83/150/217 ms capture the relevant 240 ms animation interval. `results.json` records actual sample times and verified source/destination contents.

See [capture details and replay instructions](EVIDENCE.md). The capture scripts are included. These are the composited captures referenced in that document; earlier diagnostic captures and local container tooling remain in the implementation worktree.

| Case | Before | After |
| --- | --- | --- |
| escape | [Video](before-composited/escape.mp4) · [Animation](before-composited/escape.gif) | [Video](after-composited/escape.mp4) · [Animation](after-composited/escape.gif) |
| outside | [Video](before-composited/outside.mp4) · [Animation](before-composited/outside.gif) | [Video](after-composited/outside.mp4) · [Animation](after-composited/outside.gif) |
| copy | [Video](before-composited/copy.mp4) · [Animation](before-composited/copy.gif) | [Video](after-composited/copy.mp4) · [Animation](after-composited/copy.gif) |
| move | [Video](before-composited/move.mp4) · [Animation](before-composited/move.gif) | [Video](after-composited/move.mp4) · [Animation](after-composited/move.gif) |
| noop | [Video](before-composited/noop.mp4) · [Animation](before-composited/noop.gif) | [Video](after-composited/noop.mp4) · [Animation](after-composited/noop.gif) |
| failed | [Video](before-composited/failed.mp4) · [Animation](before-composited/failed.gif) | [Video](after-composited/failed.mp4) · [Animation](after-composited/failed.gif) |
| reduced | [Video](before-composited/reduced.mp4) · [Animation](before-composited/reduced.gif) | [Video](after-composited/reduced.mp4) · [Animation](after-composited/reduced.gif) |
