# Strata — Initial Technical Direction

> This early technical assessment is retained as background. The [PRD](prd.md) is the product North Star, while the [roadmap](roadmap.md) and [work breakdown](todo.md) track delivery.

## Product vision

Build a fast, keyboard-friendly, native file manager for Linux, designed primarily for Omarchy while remaining portable to other modern Linux distributions and desktop environments.

The defining experience is:

- Miller-column-style navigation
- Folder peeking on hover
- Ultra-fast search
- Rich file previews
- A collapsible places sidebar
- Compact and airy density modes
- List and grid views
- Runtime theming
- Complete keyboard navigation

The web prototype demonstrates the intended visual language and interactions, including dynamically stacked columns, a right-side preview, a search palette, theme switching, and multiple display densities.

---

## Core interaction model

The navigation shown in the prototype is best described as **Miller columns**. Each directory in the active path receives a column, allowing users to see parent and child context simultaneously.

This should be distinguished from arbitrary split panes:

- **Miller columns:** Columns represent levels of one navigation path.
- **Split panes:** Multiple independent locations can be displayed simultaneously.

The MVP should implement Miller columns only. Arbitrary split panes can be considered after the core navigation model has been validated.

### Hover peeking

Hovering over a directory should temporarily populate the next column without committing navigation.

Expected behavior:

- Debounce hover by approximately 200 ms.
- Cancel stale directory reads immediately.
- Clicking or pressing `Enter` commits the navigation.
- Moving the pointer away restores the committed path.
- Hovering does not modify back/forward history.

This separation between temporary and committed navigation is important for preventing an unstable or unpredictable interface.

---

# Recommended technology

## Primary recommendation

Build the application as a native Linux program using:

- **Language:** Rust
- **UI toolkit:** GTK4 through `gtk4-rs`
- **Filesystem and desktop integration:** GIO
- **Styling:** Custom GTK CSS
- **Primary target:** Wayland, Hyprland, and Omarchy
- **Secondary target:** Other modern Linux desktop environments
- **Initial packaging:** Arch Linux/AUR
- **Later packaging:** Flatpak and other distro-native formats

Use plain GTK4 with custom CSS rather than making the design heavily dependent on libadwaita. Libadwaita may still be used selectively for system appearance integration or useful components, but the prototype has its own visual identity.

## Why GTK4 and Rust

Rust provides:

- Memory safety
- Strong concurrency guarantees
- Predictable native performance
- Good support for long-running background work
- A strong ecosystem for filesystem, search, parsing, and media-related tasks

GTK4 provides:

- Native Wayland support
- Virtualized list and grid widgets
- Keyboard, pointer, clipboard, and drag-and-drop handling
- Accessibility support
- Runtime CSS styling
- Mature Linux desktop integration

GIO is the decisive advantage. It provides:

- Asynchronous filesystem APIs
- File monitoring
- MIME/content-type detection
- Default application launching
- Trash support
- Mount and volume discovery
- Native icon lookup
- Clipboard and drag-and-drop integration
- File URI handling

Reimplementing these capabilities around a less integrated toolkit would consume a significant portion of the project.

## Framework comparison

| Option | Assessment |
|---|---|
| **Rust + GTK4** | Best overall balance of Linux integration, safety, performance, Wayland support, and filesystem capabilities |
| C++ + Qt/QML | Excellent UI and animation tooling; strongest alternative if exact motion design is the highest priority |
| Rust + Iced | Attractive pure-Rust option, but has weaker Linux desktop and file-manager integration |
| Rust + Slint | Good custom UI performance, but has less native desktop integration and additional licensing considerations |
| Tauri | Useful for reusing the prototype, but desktop integration and complex file operations become awkward |
| Electron | Fast for web-product iteration, but conflicts with the lightweight native performance goal |
| Quickshell/QML | Excellent for Omarchy shell components, but not the best foundation for a cross-distro file manager |

Qt 6/QML is the serious alternative. If pixel-perfect animations and rapid translation of the web prototype matter more than Rust and GIO integration, C++ with Qt/QML should be considered. It would be preferable to using immature Rust/Qt bindings for the core application.

---

# MVP scope

## 1. Navigation

The MVP should support:

- Local filesystem navigation
- Miller columns
- Current path or breadcrumb display
- Back, forward, parent, and home navigation
- Hover peeking
- Hidden-file toggle
- Live filesystem updates
- A collapsible sidebar containing:
  - Home
  - Desktop
  - Documents
  - Downloads
  - Pictures
  - Videos
  - Mounted volumes
  - User bookmarks

Remote locations are not required for the MVP.

## 2. File display

The MVP should include:

- List view
- Grid view
- Compact and airy density modes
- Name, icon, type, size, and modified date
- Sorting by common attributes
- Configurable thumbnail or icon size
- Virtualized rendering for large directories

The application must not create one permanent UI widget for every file. GTK's `ListView` and `GridView` recycle visible rows and are suitable for large directories.

## 3. Preview pane

Initial preview support should include:

- PNG, JPEG, WebP, and GIF images
- Plain text and source code
- Markdown
- First-page PDF previews
- Basic audio and video metadata
- Video thumbnails
- Directory summaries
- Generic metadata for unsupported files

Preview generation must observe strict limits:

- Never load an entire large text file.
- Decode images near the displayed resolution.
- Cancel work when the selection changes.
- Apply time and resource limits.
- Keep preview generation off the UI thread.
- Cache thumbnails according to the freedesktop thumbnail specification.

Eventually, previews for untrusted PDFs, SVGs, images, and media should be generated in a separate restricted process. File parsers are an attack surface, and malformed files should not be able to crash the main application.

## 4. Search

The MVP should expose three clear search modes:

1. Instant filtering of the currently loaded directory
2. Recursive filename search from the current directory
3. Optional file-content search

Initial implementation:

- Use `fd` or an internal parallel directory walker for filename search.
- Use `ripgrep` for content search.
- Stream results as they are found.
- Cancel the previous search immediately when the query or location changes.
- Keep result rendering virtualized.

A global indexing daemon should not be part of the MVP.

A global index introduces:

- Persistent database management
- Filesystem watchers
- inotify limits
- Exclusion rules
- Removable and network mount handling
- Stale entries
- Privacy expectations
- CPU, memory, and battery usage

A later indexed-search service could use SQLite or Tantivy. Fast recursive search should already feel immediate for many common directory trees.

The interface should communicate whether the user is filtering a loaded directory, recursively searching filenames, or searching file contents.

## 5. Essential file operations

A usable file manager must support:

- Open with the default application through GIO
- Create a file
- Create a folder
- Rename
- Copy
- Move
- Paste
- Duplicate
- Move to trash
- Permanent deletion with confirmation
- Drag and drop
- Operation progress
- Operation cancellation
- Name-conflict handling
- Symlink-safe behavior
- Read-only and permission error reporting

GIO should be used where practical for operations, trash, mounts, file URIs, content types, and desktop integration.

A root or administrator mode should not be included in the MVP.

## 6. Keyboard navigation

Proposed default shortcuts:

| Key | Action |
|---|---|
| `j` / `Down` | Select next item, or return from column header controls |
| `k` / `Up` | Select previous item, then focus column header controls |
| `h` / `Left` | Move between header controls, parent columns, then sidebar |
| `l` / `Right` / `Enter` | Open item or descend into directory |
| `Space` | Toggle or focus preview |
| `/` or `Ctrl+F` | Search |
| `Ctrl+L` | Edit current location |
| `Ctrl+H` | Toggle hidden files |
| `Ctrl+C` | Copy |
| `Ctrl+X` | Cut |
| `Ctrl+V` | Paste |
| `F2` | Rename |
| `Delete` | Move to trash |
| `Shift+Delete` | Permanently delete |
| `Ctrl+B` | Toggle sidebar |
| `Ctrl+Shift+B` | Move focus between the sidebar and previous control |
| `Ctrl+1` | List view |
| `Ctrl+2` | Grid view |
| `Ctrl++` / `Ctrl+-` | Change density or icon size |
| `Esc` | Cancel search, hover peek, dialog, or operation |

Shortcuts can become configurable later. A complete shortcut editor is not necessary for the MVP.

---

# Explicitly out of scope for the MVP

Defer the following features:

- SMB, SFTP, FTP, and cloud storage
- Arbitrary split panes
- Tabs and saved workspaces
- Archive browsing as virtual folders
- A global indexing daemon
- Advanced batch rename
- Plugin system
- Git status integration
- Root or administrator mode
- Full operation undo history
- Duplicate-file detection
- Tags and ratings
- Embedded terminal
- Full media playback
- A conventional tree view alongside Miller columns

Each of these can become a focused milestone after the primary interaction model has been validated.

---

# Proposed architecture

```text
Application
├── UI and navigation state
│   ├── Sidebar
│   ├── Miller columns
│   ├── List/grid presentation
│   ├── Search overlay
│   └── Preview pane
├── Filesystem service
│   ├── Async directory enumeration
│   ├── Metadata and MIME detection
│   ├── File monitoring
│   └── Mount discovery
├── Operation queue
│   ├── Copy/move
│   ├── Trash/delete
│   ├── Conflict resolution
│   └── Progress/cancellation
├── Search service
│   ├── Current-directory filter
│   ├── Recursive filename search
│   └── Content search
├── Preview service
│   ├── Thumbnail cache
│   ├── Text reader
│   ├── Image decoder
│   └── PDF/media helpers
└── Theme adapter
    ├── Omarchy theme
    ├── GTK/system theme
    └── Custom application theme
```

## Concurrency rules

- GTK objects must only be accessed on the main thread.
- Directory enumeration must be incremental.
- Every navigation, search, and preview request receives a cancellation token.
- CPU-heavy preview work uses a bounded worker pool.
- Concurrent thumbnail and preview jobs must be limited.
- Visible items receive priority over off-screen items.
- Expensive metadata should not be retrieved until it is needed.
- Results should be delivered to the UI in bounded batches.

The largest performance gains will come from these architectural choices rather than from the language alone.

---

# Omarchy integration

Omarchy exposes its current generated theme at:

```text
~/.local/state/omarchy/current/theme/colors.toml
```

The application can map semantic values such as:

- `background`
- `dark_background`
- `darker_background`
- `lighter_background`
- `foreground`
- `muted`
- `selection`
- `accent`
- `active_border_color`

into application GTK CSS.

Recommended behavior:

1. Detect whether Omarchy's current-theme state exists.
2. Load `colors.toml`.
3. Monitor the theme state for changes or replacement.
4. Regenerate and apply GTK CSS at runtime.
5. On non-Omarchy systems, follow GTK/system light and dark preferences.
6. Allow an application-specific TOML theme override.

Omarchy must not be a hard dependency. It should be implemented as a theme adapter and packaging target, not mixed into the core filesystem or navigation logic.

The application should use the current system icon theme rather than bundling a complete icon set.

---

# Major technical implications and risks

## Large directories

Directories containing 100,000 or more entries will expose:

- Excessive metadata calls
- Expensive sorting
- UI model replacement costs
- Thumbnail generation storms
- Memory retention

Directory results should stream in batches. Sorting and filtering should avoid repeatedly rebuilding the complete UI model.

## Filesystem correctness

Linux filesystems contain edge cases such as:

- Filenames that are not valid UTF-8
- Broken symlinks
- Symlink loops
- Permission changes during operations
- Files disappearing while being displayed or copied
- Network or removable mounts disconnecting
- Case-sensitive names
- Cross-device moves
- Sparse and extremely large files

Paths should be stored internally using native path representations. The application must not assume every filename is valid UTF-8.

## Search expectations

“Ultra-fast search” must be defined precisely:

- Filtering a loaded directory can be instant.
- Recursive filename search can be extremely fast without an index.
- Global content search cannot always be instant.
- Indexed global search requires a persistent background service.

Search modes and their scope should be visible to the user.

## Preview security and stability

A corrupt image, PDF, or video must not crash the file manager. Preview work requires:

- Cancellation
- Resource limits
- Decode-size limits
- Timeouts
- Fallback behavior
- Eventual process isolation

## File-operation reliability

Copying and moving files involves:

- Name collisions
- Permission and timestamp preservation
- Cross-filesystem operations
- Partial failures
- Cancellation cleanup
- Disk-full behavior
- Safe replacement semantics

The operation queue should be treated as a core subsystem rather than incidental UI code.

## Clipboard compatibility

Linux file managers commonly exchange file operations through URI lists and desktop-specific clipboard formats. Clipboard handling should interoperate with established file managers rather than supporting only the application's own process.

## Packaging and sandboxing

Flatpak improves distribution and isolation but complicates unrestricted filesystem access. The initial native Arch/AUR package can establish functionality first. Flatpak support should later be designed around filesystem permissions and desktop portals without silently reducing expected file-manager capabilities.

---

# Performance targets

Initial measurable targets:

- Cold start under 150–250 ms on the target machine
- First normal directory rendered in under 100 ms
- 60 FPS interaction during search and thumbnail generation
- Keyboard response within one frame
- First recursive search results in under 100 ms where possible
- A directory containing 100,000 entries remains usable
- Typical memory usage below approximately 100–150 MB
- No uncancellable background work
- No blocking filesystem or preview work on the GTK thread

Create benchmark fixtures containing:

- 1,000 files
- 10,000 files
- 100,000 files
- Deep directory trees
- Large images and text files
- Broken symlinks
- Permission-denied directories
- Rapidly changing directory contents

Performance tests should be created during the technical spike rather than after the UI is complete.

---

# Suggested delivery plan

## Phase 0: Technical spike — 1–2 weeks

Build only:

- A GTK window
- One virtualized directory list
- Async directory enumeration
- Miller-column navigation
- Hover peek with cancellation
- Basic image and text previews

Test immediately with a 100,000-file directory. This validates the riskiest interaction and performance assumptions before building the full visual shell.

### Exit criteria

- Navigation never blocks the UI thread.
- Rapid hover changes do not display stale results.
- Large-directory scrolling remains responsive.
- Preview generation is cancellable.
- Memory does not grow without bound while navigating.

## Phase 1: Navigation MVP — 3–5 weeks

- Sidebar
- Miller columns
- Back/forward history
- Keyboard navigation
- List and grid views
- Density modes
- File monitoring
- Basic settings persistence

## Phase 2: Functional file manager — 3–5 weeks

- Open/create/rename
- Copy/move/trash/delete
- Clipboard integration
- Drag and drop
- Progress and conflict dialogs
- Mounts and bookmarks

## Phase 3: Search, previews, and themes — 3–5 weeks

- Streaming recursive search
- Content search
- Thumbnail caching
- PDF and media previews
- Omarchy theme adapter
- Generic GTK/system theming
- Error, loading, and empty states

A credible solo-developer MVP is approximately **10–16 full-time weeks**. A polished replacement for an established Linux file manager is more realistically a **6–12 month project**.

---

# Initial technical stack

Recommended starting dependencies and capabilities:

```text
Rust
gtk4-rs
GIO
Custom GTK CSS
serde
toml
tracing
A bounded background worker pool
fd/ripgrep integration for initial search
Poppler for optional PDF previews
ffmpegthumbnailer for optional video previews
```

The project should avoid adding a database until a feature genuinely requires one. Basic settings can initially use a small configuration file or GSettings.

---

# First engineering milestone

The first milestone should not be a complete recreation of the web prototype.

It should be a benchmarkable native spike proving that the following remain smooth together:

1. Miller columns
2. Hover peeking
3. Aggressive cancellation
4. Incremental directory loading
5. File previews
6. Directories with 100,000 entries

If this spike succeeds, the visual design and remaining file-manager functionality can be built on a validated foundation. If it fails, the project can reevaluate its model, widget strategy, or toolkit before a large investment has been made.
