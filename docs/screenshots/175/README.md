# FileChooser rework

Before: [original PR demo](https://github.com/user-attachments/assets/e781721f-4d0e-4800-8a1b-125d1020afbb).

After, captured using disposable sample files on a private D-Bus session:

- `test-page.png`: recreated five-case Chromium test page.
- `browser-results.png`: all five browser cases completed successfully through the portal frontend.
- `open-explorer.png`, `open-multiple.png`: current Explorer chrome, metadata, and multiple selection.
- `preview.png`: native image preview and application-supplied image filter.
- `save.png`, `overwrite.png`: suggested filename, compact options, toolbar New Folder icon, and shared themed overwrite modal.
- `grid.png`: multiple selection across type groups in Grid.
- `save-light.png`: Filter, Encoding, and Compress files on one compact row in Classic Light.
- `savefiles-columns.png`: FileChooser v4 SaveFiles with Columns and choices.
- `opt-in.png`, `opt-in-light.png`: the one-time, consent-only offer in Azure Glow and Classic Light, with matching paragraph insets.
- `setup-success.png`, `setup-success-light.png`: the success state replaces the explanation with a theme-colored Lucide circle-check above the result.
- `settings-integration.png`: permanent enable/restore access in Settings → General.
- `settings-restore-before.png`, `settings-restore.png`, `settings-restore-light.png`: the configured integration dialog before and after applying the shared message-width wrapping, including Classic Light.

The opt-in captures use isolated XDG directories and a private bus; service restart/reload commands were stubbed during installation/removal testing, so the installed desktop portal was never replaced.

The browser-page captures use Chromium's File System Access API. The updated chooser captures use the dedicated client, including Classic Light, grouped Grid, and SaveFiles. See [local test instructions](../../portal-file-chooser.md#local-test-tools) to reproduce them without modifying the installed desktop portal.
