# Invalid archive error evidence — Strata #434 / PR #638

These are original, unedited GTK screenshots from private Xvfb sessions, captured only from the application window with xcompmgr compositing enabled.

- Before source: `110a433fd8c4e0787c3652550809478ef573b643`.
- After source: `3a12cc88f6b5f8e8bcf8ea2d7413ff08cdb92050`. The subsequent PR commit changes only the marquee E2E scenario.
- Matching capture environment: GTK 4.22.4, azure-glow application theme, Adwaita toolkit theme, scale 1, 1200×760 application window, Cairo renderer, xcompmgr 1.1.10.
- Both phases use identical harmless fixture bytes; `fixtures.json` records their sizes and SHA-256 checksums.
- `before/` and `after/` contain seven matching error dialogs, accessibility trees, context-menu captures, and the capture environment records.

## Reproduce

Download the files in `fixtures/` into a disposable folder. In each source build, open that folder and choose **Extract here** on each file. Capture the resulting error dialog before closing it. The text masquerading as ZIP/7z/TAR/gzip and the truncated ZIP/TAR/gzip files must produce the clear invalid-or-damaged-archive message in the after build.

Run captures under a private Xvfb display and private session/accessibility buses with isolated HOME/XDG directories. Start `xcompmgr -n` before Strata. Use the same theme, scale, toolkit, and window dimensions for both builds. Capture the application X11 window using `import -silent -screen -window <window-id> <output.png>` so transparent popup areas are composited and the display's unused margins are excluded.

The context-menu screenshots demonstrate the composited popup rendering; the code change itself changes archive error wording, not styling.
