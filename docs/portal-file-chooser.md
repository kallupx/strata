# Strata system file chooser

Strata can serve the XDG Desktop Portal FileChooser interface for portal-aware applications. Native file pickers and applications that do not use the portal are unchanged.

The first release is deliberately limited to local files and folders. X11 parent-window integration is best-effort, so the chooser may appear as a standalone modal window; Wayland applications can provide a parent handle directly.

## Per-user installation

Install Strata at a stable absolute path. The commands below use the default XDG locations and an existing installation at `~/.local/bin/strata`:

```bash
data_home="${XDG_DATA_HOME:-$HOME/.local/share}"
config_home="${XDG_CONFIG_HOME:-$HOME/.config}"
strata_executable="$(readlink -f "$HOME/.local/bin/strata")"
strata_replacement="${strata_executable//\\/\\\\}"
strata_replacement="${strata_replacement//&/\\&}"
strata_replacement="${strata_replacement//|/\\|}"

install -d "$data_home/xdg-desktop-portal/portals" \
  "$data_home/dbus-1/services" \
  "$config_home/xdg-desktop-portal"
install -m 644 portal/strata.portal \
  "$data_home/xdg-desktop-portal/portals/strata.portal"
sed "s|@STRATA_EXECUTABLE@|$strata_replacement|" \
  portal/org.freedesktop.impl.portal.desktop.strata.service.in \
  > "$data_home/dbus-1/services/org.freedesktop.impl.portal.desktop.strata.service"
chmod 644 "$data_home/dbus-1/services/org.freedesktop.impl.portal.desktop.strata.service"
```

The generated D-Bus service must contain an absolute `Exec=` path. If that path contains whitespace, install Strata somewhere else; D-Bus service-file argument parsing is not shell quoting.

Open `$config_home/xdg-desktop-portal/portals.conf`, preserve its existing `[preferred]` section and settings, and merge Strata into the FileChooser preference:

```ini
[preferred]
org.freedesktop.impl.portal.FileChooser=strata;<existing-backend>;
```

Replace `<existing-backend>` with the backend already configured for the desktop, such as `gtk` or `gnome`. Do not install the placeholder literally and do not replace unrelated portal preferences. The archive's `portal/portals.conf` is an example, not a complete desktop configuration.

Restart the frontend so it rereads portal metadata and preferences:

```bash
systemctl --user restart xdg-desktop-portal.service
```

On a desktop that does not manage the frontend as a systemd user unit, log out and back in instead.

## Verification

Confirm that D-Bus can activate Strata and that it advertises FileChooser version 4:

```bash
gdbus introspect --session \
  --dest org.freedesktop.impl.portal.desktop.strata \
  --object-path /org/freedesktop/portal/desktop
gdbus call --session \
  --dest org.freedesktop.impl.portal.desktop.strata \
  --object-path /org/freedesktop/portal/desktop \
  --method org.freedesktop.DBus.Properties.Get \
  org.freedesktop.impl.portal.FileChooser version
```

The second command should report `uint32 4`. Then open or save a file from a portal-aware application. Only local locations appear in this initial picker; entering a remote URI shows an unsupported-location error.

Portal backend selection happens before a request is sent. Keeping the existing backend after `strata;` lets the frontend choose it when Strata's `.portal` metadata is absent. It does not provide live failover if an already-selected Strata backend crashes during a request.

## Uninstall

Remove the Strata metadata and activation service:

```bash
data_home="${XDG_DATA_HOME:-$HOME/.local/share}"
rm -f "$data_home/xdg-desktop-portal/portals/strata.portal" \
  "$data_home/dbus-1/services/org.freedesktop.impl.portal.desktop.strata.service"
```

Edit `${XDG_CONFIG_HOME:-$HOME/.config}/xdg-desktop-portal/portals.conf`, remove `strata;` from the FileChooser preference while retaining the previous backend, then restart the portal:

```bash
systemctl --user restart xdg-desktop-portal.service
```

For a complete Strata uninstall, also remove the application binary and desktop entry as described in the main installation guide.
