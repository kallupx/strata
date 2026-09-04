// SPDX-License-Identifier: GPL-3.0-or-later

use std::{
    cell::{Cell, RefCell},
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
    rc::{Rc, Weak},
    time::Duration,
};

use crate::{
    app::navigation::{EntryInsertion, EntrySplice, NavigationPath, NavigationState, sort_entries},
    model::{FileEntry, Location, SortDirection, SortKey, ViewPreferences},
    services::{
        ArchiveFormat, CompressRequest, CreateDirectoryRequest, CreateFileRequest, DeleteRequest,
        DirectoryChange, DirectoryEvent, DirectoryRequest, ExtractRequest, FileSource, LoadHandle,
        LocationValidationError, MetadataOutcome, MetadataRequest, OperationEvent,
        OperationProvider, OperationRequestId, PasteItem, PasteRequest, RenameRequest, RequestId,
        RestoreRequest, RestoreSource, TransferConflict, validate_basename,
        validate_uri_credentials,
    },
};

/// Caps a normal directory load at this project's own documented performance baseline for
/// 100,000 entries (docs/performance-baseline.md: 3,755 ms, 286 MiB) -- past this, per-batch
/// merge cost grows enough that browsing stops feeling responsive.
const MAX_DIRECTORY_ENTRIES: usize = 100_000;
const DIRECTORY_LOAD_TIME_BUDGET: Duration = Duration::from_secs(10);

/// GIO batch size for local directories. Fewer, larger batches cut the
/// per-batch merge, selection scan, and GTK splice count ~4x on large
/// listings. Remote (GVfs) locations keep small batches so first paint
/// doesn't wait out high per-file latency on slow links.
const NATIVE_DIRECTORY_BATCH_SIZE: usize = 512;
const REMOTE_DIRECTORY_BATCH_SIZE: usize = 128;

/// A hover peek only ever displays a handful of entries (`PeekBehavior::item_limit`), so it
/// needs far less headroom than a full directory load -- just enough to survive hidden-file
/// filtering, not enough to enumerate an entire large directory for a preview that discards
/// nearly all of it.
const PEEK_MAX_ENTRIES: usize = 64;
const PEEK_TIME_BUDGET: Duration = Duration::from_secs(3);

#[derive(Clone, Debug)]
pub struct BrowserColumnSnapshot {
    pub location: Location,
    pub count: usize,
    pub selected_positions: Vec<usize>,
    pub loading: bool,
    pub error: Option<String>,
    pub truncated: bool,
}

#[derive(Clone, Debug)]
pub enum BrowserEvent {
    Reset,
    ColumnsTruncated {
        len: usize,
    },
    ColumnAdded {
        depth: usize,
        location: Location,
    },
    EntriesInserted {
        depth: usize,
        insertions: Vec<EntryInsertion>,
    },
    EntriesReplaced {
        depth: usize,
        count: usize,
    },
    /// A contiguous range already installed in authoritative state. Views
    /// borrow that range during synchronous dispatch instead of receiving a
    /// deep clone of every entry.
    EntriesPublished {
        depth: usize,
        position: usize,
        count: usize,
    },
    SortingStarted {
        depth: usize,
    },
    SortingFinished {
        depth: usize,
    },
    EntriesSpliced {
        depth: usize,
        splices: Vec<EntrySplice>,
        selected: Option<usize>,
    },
    /// Size/mtime arrivals for already-rendered rows: positions with their
    /// refreshed entries, for an in-place same-count model refresh. The order
    /// never changes here; sorting by size or date runs its own full pass.
    MetadataFilled {
        depth: usize,
        updates: Vec<(usize, FileEntry)>,
    },
    ColumnReloaded {
        depth: usize,
    },
    HiddenToggled {
        show_hidden: bool,
    },
    LoadFinished {
        depth: usize,
        truncated: bool,
    },
    LoadFailed {
        depth: usize,
        message: String,
    },
    PeekStarted {
        location: Location,
    },
    PeekEntriesAdded {
        entries: Vec<FileEntry>,
    },
    PeekFinished,
    PeekFailed {
        message: String,
    },
    PeekClosed,
    FocusChanged {
        depth: usize,
        position: Option<usize>,
    },
    SelectionSetChanged {
        depth: usize,
        positions: Vec<usize>,
        focused: usize,
        take_focus: bool,
    },
    PreviewRequested {
        entry: FileEntry,
    },
    OpenRequested {
        location: Location,
    },
    RenameCompleted,
    RenameFailed {
        message: String,
    },
    TransferStarted {
        total: usize,
        moving: bool,
    },
    TransferProgress {
        completed: usize,
        total: usize,
    },
    TransferFinished {
        moved_locations: Vec<Location>,
    },
    DeletionStarted {
        total: usize,
    },
    DeletionProgress {
        completed: usize,
        total: usize,
    },
    DeletionFinished,
    RestorationStarted {
        total: usize,
    },
    RestorationProgress {
        completed: usize,
        total: usize,
    },
    RestorationFinished,
    OperationFailed {
        message: String,
    },
    OperationCompletedWithErrors {
        message: String,
        /// Entries a retry with `permanent: true` would likely delete
        /// successfully, e.g. ones that failed only because this location
        /// doesn't support Trash. Always empty for a restore failure.
        retryable_locations: Vec<Location>,
        has_non_retryable_failures: bool,
    },
    OperationCancelled {
        completed: usize,
        failed: usize,
        not_attempted: usize,
        affected_locations: HashSet<Location>,
    },
    NavigationRejected {
        parent_depth: usize,
        error: LocationValidationError,
    },
    LocationNavigationRejected {
        error: LocationValidationError,
    },
    EmptyTrashRequested,
    ArchiveStarted {
        total: usize,
    },
    ArchiveProgress {
        completed: usize,
        total: usize,
    },
    ArchiveCompleted {
        select_name: String,
    },
    TransferCompleted,
}

/// Observers receive the event by reference during synchronous dispatch:
/// listing payloads (`Vec<FileEntry>`) move exactly once from the provider
/// into authoritative state and the single emitted event, and fan-out to
/// every observer borrows instead of deep-cloning per observer. Consumers
/// that retain data clone only the small field they store. The observer
/// list itself is cloned before dispatch so add/remove/reentrant emission
/// stays safe.
type Observer = Rc<dyn Fn(&BrowserEvent)>;
type PreferencesObserver = Rc<dyn Fn(ViewPreferences)>;

const MAX_INCREMENTAL_OPERATION_UPDATES: usize = 64;

#[derive(Default)]
struct TrashUndoState {
    generation: u64,
    locations: Vec<Location>,
    claimed: bool,
}

// Undo follows the latest operation across every Strata window on the GTK main thread.
thread_local! {
    static PENDING_TRASH_UNDO: RefCell<TrashUndoState> = RefCell::new(TrashUndoState::default());
}

fn replace_pending_trash_undo(locations: Vec<Location>) {
    PENDING_TRASH_UNDO.with(|pending| {
        let generation = pending.borrow().generation.saturating_add(1);
        pending.replace(TrashUndoState {
            generation,
            locations,
            claimed: false,
        });
    });
}

fn claim_pending_trash_undo() -> Option<(u64, Vec<Location>)> {
    PENDING_TRASH_UNDO.with(|pending| {
        let mut pending = pending.borrow_mut();
        if pending.claimed || pending.locations.is_empty() {
            return None;
        }
        pending.claimed = true;
        Some((pending.generation, pending.locations.clone()))
    })
}

fn mark_trash_undo_restored(generation: u64, location: &Location) {
    PENDING_TRASH_UNDO.with(|pending| {
        let mut pending = pending.borrow_mut();
        if pending.generation == generation {
            pending.locations.retain(|candidate| candidate != location);
        }
    });
}

fn finish_trash_undo(generation: u64, completed: bool) {
    PENDING_TRASH_UNDO.with(|pending| {
        let mut pending = pending.borrow_mut();
        if pending.generation == generation {
            if completed {
                pending.locations.clear();
            }
            pending.claimed = false;
        }
    });
}

#[cfg(test)]
fn pending_trash_undo() -> Vec<Location> {
    PENDING_TRASH_UNDO.with(|pending| pending.borrow().locations.clone())
}

/// Settles scrolling before asking for viewport metadata, so a fling never
/// stats hundreds of rows it never shows.
const METADATA_FILL_DEBOUNCE: Duration = Duration::from_millis(100);
/// Bounds one metadata fill; partial results still apply, the rest retries on
/// its next bind.
const METADATA_FILL_TIME_BUDGET: Duration = Duration::from_secs(5);
/// Defensive cap per depth: the UI only ever asks for its visible window.
const MAX_PENDING_FILL_LOCATIONS: usize = 1024;

/// Accumulates this many entries before flushing early: first paint applies
/// at once, later batches merge, scan, and splice in groups of four.
/// Remote loads only; native loads stage instead (see `StagingLoad`).
const COALESCE_ENTRIES: usize = 2048;
/// Bounds one remote progressive flush: a slow link must not turn one timer
/// fire into a multi-frame GTK mutation.
const REMOTE_FLUSH_CAP: usize = 512;
/// Maximum latency for remote progressive rows: later batches flush on the
/// next idle/frame instead of waiting solely for the count threshold.
const REMOTE_FLUSH_DELAY: Duration = Duration::from_millis(50);
/// Rows published synchronously with a staged load or sort; the rest stream
/// from idle callbacks inside an 8 ms work budget.
const FIRST_PUBLISH_COUNT: usize = 128;
/// Loads at or below this size publish in one synchronous replace.
const STAGE_INLINE_LIMIT: usize = 512;
/// Snapshots at or below this size sort synchronously on the calling
/// thread: sub-millisecond work that needs no off-thread hop and no main
/// context. Larger snapshots sort in a blocking worker.
const SORT_INLINE_LIMIT: usize = 2048;
/// Rows per publication tail callback.
const PUBLISH_TAIL_CHUNK: usize = 2048;
/// Main-thread work budget per publication tail callback.
const PUBLISH_SLICE_BUDGET: Duration = Duration::from_millis(8);

/// Last selection event emitted per depth on the batch path, keyed by request
/// so a new load re-emits even when it selects the same rows. Lets background
/// batches skip the redundant per-pane selection refresh (and its scroll)
/// when nothing moved.
type BatchSelectionState = HashMap<usize, (RequestId, Vec<usize>, usize)>;

/// One bound row's viewport fill request: the stable location plus the source
/// position it occupied when bound, so fills apply in O(requested rows)
/// after validating the row has not moved.
struct ViewportTarget {
    position: usize,
    location: Location,
}

/// A native initial load in flight. Identity batches accumulate here with no
/// merge walk and no UI events; monitor deltas arriving mid-stage queue for
/// one reconcile instead of racing the snapshot. Removed locations filter
/// later batches, so a removed entry is never resurrected by a stale batch
/// while an upserted one still lands.
struct StagingLoad {
    request_id: RequestId,
    entries: Vec<FileEntry>,
    removed: HashSet<Location>,
    deltas: Vec<(Location, DirectoryChange)>,
}

/// A native load sorting off-thread after enumeration finished. Deltas
/// arriving here queue for the completion's silent reconcile.
struct SortingLoad {
    request_id: RequestId,
    deltas: Vec<(Location, DirectoryChange)>,
}

/// Terminal event owed after a staged publication's final tail.
enum PublishTerminal {
    LoadFinished { truncated: bool },
    SortingFinished,
}

/// A staged publication streaming to the UI: the prefix is already in the
/// model, and idle callbacks append contiguous tails inside a work budget.
/// Chunks clone from authoritative state at fire time, so no full-vector
/// copy ever crosses the publish path. Selection and the terminal event
/// wait for the final tail, so no out-of-range selection and no premature
/// completion is ever published.
struct StagedPublish {
    request_id: RequestId,
    published: usize,
    total: usize,
    focused: Option<usize>,
    positions: Vec<usize>,
    terminal: PublishTerminal,
}

pub struct Browser {
    source: Rc<dyn FileSource>,
    state: RefCell<NavigationState>,
    loads: RefCell<Vec<LoadHandle>>,
    monitors: RefCell<Vec<Option<LoadHandle>>>,
    metadata_pending: RefCell<HashMap<usize, Vec<ViewportTarget>>>,
    metadata_timer: RefCell<Option<gio::glib::SourceId>>,
    staging: RefCell<HashMap<usize, StagingLoad>>,
    /// Native loads whose snapshot is sorting off-thread. Monitor deltas
    /// arriving mid-sort queue here for the same silent reconcile.
    sorting: RefCell<HashMap<usize, SortingLoad>>,
    staged_publishes: RefCell<HashMap<usize, StagedPublish>>,
    publish_timer: RefCell<Option<gio::glib::SourceId>>,
    remote_flush_timer: RefCell<Option<gio::glib::SourceId>>,
    metadata_loads: RefCell<HashMap<usize, LoadHandle>>,
    /// Stable `(position, Location)` tokens per in-flight viewport fill, so
    fill_tokens: RefCell<HashMap<RequestId, Vec<(usize, Location)>>>,
    /// Full-column sort fills, kept apart from viewport fills so a viewport
    /// settle timer can never overwrite or cancel an active full sort.
    sort_loads: RefCell<HashMap<usize, LoadHandle>>,
    coalesce_pending: RefCell<HashMap<usize, (RequestId, Vec<FileEntry>)>>,
    sort_awaiting_fill: RefCell<Option<(u64, usize, RequestId, ViewPreferences)>>,
    last_batch_selection: RefCell<BatchSelectionState>,
    peek_load: RefCell<Option<LoadHandle>>,
    validation_load: RefCell<Option<LoadHandle>>,
    validation_generation: Cell<u64>,
    operation_provider: RefCell<Option<Rc<dyn OperationProvider>>>,
    operation_load: RefCell<Option<LoadHandle>>,
    current_operation: Cell<Option<OperationRequestId>>,
    transfer_operation: Cell<Option<bool>>,
    deletion_operation: Cell<bool>,
    deletion_permanent: Cell<bool>,
    restoration_operation: Cell<bool>,
    undo_restoration: RefCell<Option<(u64, Vec<Location>)>>,
    next_request: Cell<u64>,
    pending_sort: Cell<Option<(u64, usize)>>,
    preferences: Cell<ViewPreferences>,
    observers: RefCell<Vec<Observer>>,
    preferences_observers: RefCell<Vec<PreferencesObserver>>,
}

impl Browser {
    #[cfg(test)]
    pub fn new(source: Rc<dyn FileSource>) -> Rc<Self> {
        Self::with_preferences(source, ViewPreferences::default())
    }

    pub fn with_preferences(source: Rc<dyn FileSource>, preferences: ViewPreferences) -> Rc<Self> {
        Rc::new(Self {
            source,
            state: RefCell::new(NavigationState::with_preferences(preferences)),
            loads: RefCell::new(Vec::new()),
            monitors: RefCell::new(Vec::new()),
            metadata_pending: RefCell::new(HashMap::new()),
            metadata_timer: RefCell::new(None),
            staging: RefCell::new(HashMap::new()),
            sorting: RefCell::new(HashMap::new()),
            staged_publishes: RefCell::new(HashMap::new()),
            publish_timer: RefCell::new(None),
            remote_flush_timer: RefCell::new(None),
            metadata_loads: RefCell::new(HashMap::new()),
            fill_tokens: RefCell::new(HashMap::new()),
            sort_loads: RefCell::new(HashMap::new()),
            coalesce_pending: RefCell::new(HashMap::new()),
            sort_awaiting_fill: RefCell::new(None),
            last_batch_selection: RefCell::new(HashMap::new()),
            peek_load: RefCell::new(None),
            validation_load: RefCell::new(None),
            validation_generation: Cell::new(0),
            operation_provider: RefCell::new(None),
            operation_load: RefCell::new(None),
            current_operation: Cell::new(None),
            transfer_operation: Cell::new(None),
            deletion_operation: Cell::new(false),
            deletion_permanent: Cell::new(false),
            restoration_operation: Cell::new(false),
            undo_restoration: RefCell::new(None),
            next_request: Cell::new(1),
            pending_sort: Cell::new(None),
            preferences: Cell::new(preferences),
            observers: RefCell::new(Vec::new()),
            preferences_observers: RefCell::new(Vec::new()),
        })
    }

    pub fn observe(&self, observer: impl Fn(&BrowserEvent) + 'static) {
        self.observers.borrow_mut().push(Rc::new(observer));
    }

    pub fn clear_observer(&self) {
        self.observers.borrow_mut().clear();
    }

    pub fn preferences(&self) -> ViewPreferences {
        self.preferences.get()
    }

    pub fn observe_preferences(&self, observer: impl Fn(ViewPreferences) + 'static) {
        self.preferences_observers
            .borrow_mut()
            .push(Rc::new(observer));
    }

    fn notify_preferences_observers(&self) {
        let preferences = self.preferences.get();
        for observer in self.preferences_observers.borrow().iter() {
            observer(preferences);
        }
    }

    pub fn set_operation_provider(&self, provider: Rc<dyn OperationProvider>) {
        self.operation_provider.replace(Some(provider));
    }

    pub fn navigate_input(self: &Rc<Self>, input: &str) -> Result<(), LocationValidationError> {
        let input = input.trim();
        if input.is_empty() {
            return Err(LocationValidationError::Empty);
        }

        if let Some(message) = unsupported_shorthand_message(input) {
            return Err(LocationValidationError::UnsupportedShorthand(
                message.to_owned(),
            ));
        }
        if let Some(current) = self
            .active_location()
            .filter(|current| current.display_path() == input)
        {
            self.navigate_validated(current);
            return Ok(());
        }
        let location = location_from_input(input)?;
        if location.native_path().is_some() && !location.is_absolute_native() {
            return Err(LocationValidationError::NotAbsolute);
        }
        if location.native_path().is_some() {
            self.source.validate_location(&location)?;
            self.navigate(location);
        } else {
            self.navigate_validated(location);
        }
        Ok(())
    }

    fn navigate_validated(self: &Rc<Self>, location: Location) {
        let generation = self.validation_generation.get().saturating_add(1);
        self.validation_generation.set(generation);
        self.validation_load.borrow_mut().take();
        let weak = Rc::downgrade(self);
        let pending_location = location.clone();
        let emit = Rc::new(move |result| {
            let Some(browser) = weak.upgrade() else {
                return;
            };
            if browser.validation_generation.get() != generation {
                return;
            }
            match result {
                Ok(()) => browser.navigate(pending_location.clone()),
                Err(error) => browser.emit(BrowserEvent::LocationNavigationRejected { error }),
            }
        });
        let load = self.source.validate_location_async(location, emit);
        self.validation_load.replace(Some(load));
    }

    pub fn active_location(&self) -> Option<Location> {
        self.state.borrow().active_location()
    }

    pub fn active_depth(&self) -> Option<usize> {
        self.state.borrow().active_depth()
    }

    pub fn location_at(&self, depth: usize) -> Option<Location> {
        self.state.borrow().location_at(depth)
    }

    pub fn focus_active(&self) {
        let focus = self.state.borrow().active_focus();
        if let Some((depth, position)) = focus {
            self.emit(BrowserEvent::FocusChanged { depth, position });
        }
    }

    /// Navigates directly for native paths and validates URI locations first so mountable
    /// locations can be mounted by the UI before loading them.
    pub(crate) fn navigate_location(self: &Rc<Self>, location: Location) {
        if location.native_path().is_some() {
            self.navigate(location);
        } else {
            self.navigate_validated(location);
        }
    }

    pub fn navigate(self: &Rc<Self>, location: Location) {
        self.validation_generation
            .set(self.validation_generation.get().saturating_add(1));
        self.validation_load.borrow_mut().take();
        if self.active_location().as_ref() == Some(&location) {
            return;
        }
        self.close_peek();
        self.loads.borrow_mut().clear();
        self.monitors.borrow_mut().clear();
        self.cancel_deferred_work();
        let request_id = self.new_request_id();
        self.state
            .borrow_mut()
            .navigate(location.clone(), request_id);
        self.emit(BrowserEvent::Reset);
        self.emit(BrowserEvent::ColumnAdded {
            depth: 0,
            location: location.clone(),
        });
        self.emit(BrowserEvent::FocusChanged {
            depth: 0,
            position: None,
        });
        self.start_load(0, location, request_id);
    }

    pub fn descend(self: &Rc<Self>, parent_depth: usize, location: Location) {
        self.descend_with_selection(parent_depth, location, false);
    }

    fn descend_with_selection(
        self: &Rc<Self>,
        parent_depth: usize,
        location: Location,
        select_first_on_load: bool,
    ) {
        self.validation_generation
            .set(self.validation_generation.get().saturating_add(1));
        self.validation_load.borrow_mut().take();
        if self.is_open_child(parent_depth, &location) {
            return;
        }
        self.close_peek();
        if location.native_path().is_some() {
            if let Err(error) = self.source.validate_location(&location) {
                self.emit(BrowserEvent::NavigationRejected {
                    parent_depth,
                    error,
                });
                self.focus_active();
                return;
            }
            self.descend_validated(parent_depth, location, select_first_on_load);
            return;
        }

        let generation = self.validation_generation.get().saturating_add(1);
        self.validation_generation.set(generation);
        self.validation_load.borrow_mut().take();
        let weak = Rc::downgrade(self);
        let pending_location = location.clone();
        let parent_location = self.location_at(parent_depth);
        let emit = Rc::new(move |result| {
            let Some(browser) = weak.upgrade() else {
                return;
            };
            if browser.validation_generation.get() != generation
                || browser.location_at(parent_depth) != parent_location
            {
                return;
            }
            match result {
                Ok(()) => browser.descend_validated(
                    parent_depth,
                    pending_location.clone(),
                    select_first_on_load,
                ),
                Err(error) => {
                    browser.emit(BrowserEvent::NavigationRejected {
                        parent_depth,
                        error,
                    });
                    browser.focus_active();
                }
            }
        });
        let load = self.source.validate_location_async(location, emit);
        self.validation_load.replace(Some(load));
    }

    fn descend_validated(
        self: &Rc<Self>,
        parent_depth: usize,
        location: Location,
        select_first_on_load: bool,
    ) {
        let request_id = self.new_request_id();
        let mut state = self.state.borrow_mut();
        if !state.descend(parent_depth, location.clone(), request_id) {
            return;
        }
        if select_first_on_load {
            state.select_first_on_load(parent_depth + 1);
        }
        drop(state);

        let retained = parent_depth + 1;
        self.loads.borrow_mut().truncate(retained);
        self.monitors.borrow_mut().truncate(retained);
        self.truncate_deferred_from(retained);
        self.emit(BrowserEvent::ColumnsTruncated { len: retained });
        self.emit(BrowserEvent::ColumnAdded {
            depth: retained,
            location: location.clone(),
        });
        self.emit(BrowserEvent::FocusChanged {
            depth: retained,
            position: None,
        });
        self.start_load(retained, location, request_id);
    }

    pub fn begin_peek(self: &Rc<Self>, origin_depth: usize, location: Location) {
        self.close_peek();
        if self.is_open_child(origin_depth, &location) {
            return;
        }
        let request_id = self.new_request_id();
        if !self
            .state
            .borrow_mut()
            .begin_peek(origin_depth, location.clone(), request_id)
        {
            return;
        }

        self.emit(BrowserEvent::PeekStarted {
            location: location.clone(),
        });
        let weak: Weak<Self> = Rc::downgrade(self);
        let emit = Rc::new(move |event| {
            if let Some(browser) = weak.upgrade() {
                browser.handle_directory_event(event);
            }
        });
        // Peeks stay small and show metadata immediately, so they keep the
        // old always-stat behavior instead of the streaming split.
        let handle = self.source.enumerate(
            DirectoryRequest {
                id: request_id,
                location,
                batch_size: 128,
                include_metadata: true,
                max_entries: PEEK_MAX_ENTRIES,
                time_budget: PEEK_TIME_BUDGET,
            },
            emit,
        );
        self.peek_load.replace(Some(handle));
    }

    pub fn close_peek(&self) -> bool {
        self.peek_load.take();
        let closed = self.state.borrow_mut().clear_peek();
        if closed {
            self.emit(BrowserEvent::PeekClosed);
        }
        closed
    }

    pub fn escape(self: &Rc<Self>) {
        if self.close_peek() {
            return;
        }

        let closed = self.state.borrow_mut().close_deepest();
        if let Some((depth, position)) = closed {
            let len = depth + 1;
            self.loads.borrow_mut().truncate(len);
            self.monitors.borrow_mut().truncate(len);
            self.truncate_deferred_from(len);
            self.emit(BrowserEvent::ColumnsTruncated { len });
            self.emit(BrowserEvent::FocusChanged { depth, position });
        }
    }

    pub fn close_column(self: &Rc<Self>, depth: usize) {
        self.close_peek();
        let closed = self.state.borrow_mut().close_from(depth);
        if let Some((parent_depth, position)) = closed {
            self.loads.borrow_mut().truncate(depth);
            self.monitors.borrow_mut().truncate(depth);
            self.truncate_deferred_from(depth);
            self.emit(BrowserEvent::ColumnsTruncated { len: depth });
            self.emit(BrowserEvent::FocusChanged {
                depth: parent_depth,
                position,
            });
        }
    }

    pub fn commit_peek(self: &Rc<Self>) {
        let target = self.state.borrow().peek_target();
        if let Some((origin_depth, location)) = target {
            self.close_peek();
            self.descend(origin_depth, location);
        }
    }

    pub fn set_sort_key(self: &Rc<Self>, depth: usize, sort_key: SortKey) {
        self.apply_column_preferences(depth, move |preferences| preferences.sort_key = sort_key);
    }

    pub fn set_sort(
        self: &Rc<Self>,
        depth: usize,
        sort_key: SortKey,
        sort_direction: SortDirection,
    ) {
        self.apply_column_preferences(depth, move |preferences| {
            preferences.sort_key = sort_key;
            preferences.sort_direction = sort_direction;
        });
    }

    pub fn set_sort_direction(self: &Rc<Self>, depth: usize, sort_direction: SortDirection) {
        self.apply_column_preferences(depth, move |preferences| {
            preferences.sort_direction = sort_direction;
        });
    }

    pub fn set_folders_first(self: &Rc<Self>, depth: usize, folders_first: bool) {
        self.apply_column_preferences(depth, move |preferences| {
            preferences.folders_first = folders_first;
        });
    }

    pub fn toggle_hidden(self: &Rc<Self>) {
        let mut preferences = self.preferences.get();
        preferences.show_hidden = !preferences.show_hidden;
        self.preferences.set(preferences);
        self.notify_preferences_observers();

        self.close_peek();
        self.state
            .borrow_mut()
            .set_show_hidden(preferences.show_hidden);
        self.emit(BrowserEvent::HiddenToggled {
            show_hidden: preferences.show_hidden,
        });
    }

    fn apply_column_preferences(
        self: &Rc<Self>,
        depth: usize,
        update: impl FnOnce(&mut ViewPreferences) + 'static,
    ) {
        if self.state.borrow().column_preferences(depth).is_none() {
            return;
        }
        let generation = self
            .pending_sort
            .get()
            .map_or(1, |(generation, _)| generation.saturating_add(1));
        if let Some((_, previous_depth)) = self.pending_sort.replace(Some((generation, depth))) {
            self.emit(BrowserEvent::SortingFinished {
                depth: previous_depth,
            });
        }
        self.emit(BrowserEvent::SortingStarted { depth });
        let weak = Rc::downgrade(self);
        gio::glib::timeout_add_local_once(Duration::from_millis(16), move || {
            if let Some(browser) = weak.upgrade() {
                browser.apply_debounced_sort(depth, generation, update);
            }
        });
    }

    fn apply_debounced_sort(
        self: &Rc<Self>,
        depth: usize,
        generation: u64,
        update: impl FnOnce(&mut ViewPreferences),
    ) {
        if self.pending_sort.get() != Some((generation, depth)) {
            return;
        }
        let result = {
            let mut state = self.state.borrow_mut();
            let Some(mut preferences) = state.column_preferences(depth) else {
                drop(state);
                self.pending_sort.set(None);
                self.emit(BrowserEvent::SortingFinished { depth });
                return;
            };
            update(&mut preferences);
            // Size and date sorts need the metadata the streaming
            // enumeration skipped. Fill the whole column first behind the
            // sort spinner instead of sorting placeholders.
            let targets = state.column_unknown_metadata(depth).unwrap_or_default();
            if matches!(preferences.sort_key, SortKey::Size | SortKey::Modified)
                && !targets.is_empty()
            {
                drop(state);
                self.request_sort_fill(depth, generation, preferences, targets);
                return;
            }
            let result = state.apply_sort_preferences(depth, preferences);
            self.preferences.set(preferences);
            let request_id = state.request_id_for_depth(depth);
            let total = state.columns.get(depth).map(|column| column.entries.len());
            result.map(|(focused, positions)| (request_id, total, focused, positions))
        };
        self.notify_preferences_observers();
        if let Some((request_id, total, focused, positions)) = result {
            // Staged publication carries the new order; the sorting
            // terminal waits for its final tail.
            if let (Some(request_id), Some(total)) = (request_id, total) {
                self.publish_staged(
                    depth,
                    request_id,
                    total,
                    focused,
                    positions,
                    PublishTerminal::SortingFinished,
                );
            } else {
                self.pending_sort.set(None);
                self.emit(BrowserEvent::SortingFinished { depth });
            }
        } else {
            self.pending_sort.set(None);
            self.emit(BrowserEvent::SortingFinished { depth });
        }
    }

    pub fn can_go_back(&self) -> bool {
        self.state.borrow().can_go_back()
    }

    pub fn can_go_forward(&self) -> bool {
        self.state.borrow().can_go_forward()
    }

    pub fn can_go_parent(&self) -> bool {
        self.state.borrow().can_go_parent()
    }

    pub fn back(self: &Rc<Self>) {
        let target = self.state.borrow_mut().go_back();
        if let Some(target) = target {
            self.restore_path(target);
        }
    }

    pub fn forward(self: &Rc<Self>) {
        let target = self.state.borrow_mut().go_forward();
        if let Some(target) = target {
            self.restore_path(target);
        }
    }

    pub fn parent(self: &Rc<Self>) {
        let target = self.state.borrow_mut().go_parent();
        if let Some(target) = target {
            self.restore_path(target);
        }
    }

    pub fn select(&self, depth: usize, position: usize) {
        let selected = self.state.borrow_mut().select(depth, position);
        if selected {
            self.emit(BrowserEvent::FocusChanged {
                depth,
                position: Some(position),
            });
        }
    }

    pub fn entry_at(&self, depth: usize, position: usize) -> Option<FileEntry> {
        self.state.borrow().entry_at(depth, position)
    }

    pub fn with_entries<R>(
        &self,
        depth: usize,
        range: std::ops::Range<usize>,
        read: impl FnOnce(&[FileEntry]) -> R,
    ) -> Option<R> {
        let state = self.state.borrow();
        let entries = &state.columns.get(depth)?.entries;
        Some(read(entries.get(range)?))
    }

    pub fn column_preferences(&self, depth: usize) -> Option<ViewPreferences> {
        self.state.borrow().column_preferences(depth)
    }

    pub fn column_snapshot(&self, depth: usize) -> Option<BrowserColumnSnapshot> {
        let state = self.state.borrow();
        let column = state.columns.get(depth)?;
        Some(BrowserColumnSnapshot {
            location: column.location.clone(),
            count: column.entries.len(),
            selected_positions: state.selected_positions(depth),
            loading: column.load_state == crate::app::navigation::LoadState::Loading,
            error: match &column.load_state {
                crate::app::navigation::LoadState::Error(message) => Some(message.clone()),
                _ => None,
            },
            truncated: column.truncated,
        })
    }

    pub fn focused_item(&self) -> Option<(usize, usize, FileEntry)> {
        self.state.borrow().focused_entry()
    }

    pub fn rename_item(&self) -> Option<(usize, usize, FileEntry)> {
        let state = self.state.borrow();
        if let Some(focused) = state.focused_entry() {
            return Some(focused);
        }
        let depth = state.active_depth()?.checked_sub(1)?;
        let position = state.active_child_position(depth)?;
        let entry = state.entry_at(depth, position)?;
        Some((depth, position, entry))
    }

    pub fn focused_entry(&self) -> Option<FileEntry> {
        self.focused_item().map(|(_, _, entry)| entry)
    }

    pub fn selected_entries(&self) -> Vec<FileEntry> {
        self.state.borrow().selected_entries()
    }
    pub fn selected_positions(&self, depth: usize) -> Vec<usize> {
        self.state.borrow().selected_positions(depth)
    }

    pub fn deletion_entries(&self) -> Vec<FileEntry> {
        let state = self.state.borrow();
        let selected = state.selected_entries();
        if !selected.is_empty() {
            return selected;
        }

        let Some(parent_depth) = state.active_depth().and_then(|depth| depth.checked_sub(1)) else {
            return Vec::new();
        };
        let Some(position) = state.active_child_position(parent_depth) else {
            return Vec::new();
        };
        state.entry_at(parent_depth, position).into_iter().collect()
    }

    pub fn set_selection(&self, depth: usize, positions: &[usize], focused: Option<usize>) {
        let mut state = self.state.borrow_mut();
        if state.set_selection(depth, positions, focused) {
            tracing::debug!(
                depth,
                selected = state.selected_count(),
                "selection changed"
            );
        }
    }

    pub fn select_all(&self, depth: usize) {
        let count = self
            .state
            .borrow()
            .columns
            .get(depth)
            .map_or(0, |column| column.entries.len());
        if count == 0 {
            return;
        }
        let positions: Vec<_> = (0..count).collect();
        let focused = count - 1;
        if self
            .state
            .borrow_mut()
            .set_selection(depth, &positions, Some(focused))
        {
            self.emit(BrowserEvent::SelectionSetChanged {
                depth,
                positions,
                focused,
                take_focus: true,
            });
        }
    }

    pub fn active_child_position(&self, depth: usize) -> Option<usize> {
        self.state.borrow().active_child_position(depth)
    }

    pub fn rename(self: &Rc<Self>, entry: FileEntry, new_name: String) {
        if let Err(message) = validate_basename(&new_name) {
            self.emit(BrowserEvent::RenameFailed {
                message: message.to_owned(),
            });
            return;
        }
        let Some(provider) = self.operation_provider.borrow().clone() else {
            self.emit(BrowserEvent::RenameFailed {
                message: "File operations are unavailable".to_owned(),
            });
            return;
        };
        let request_id = self.begin_operation();
        let refresh_locations = entry.location.parent().into_iter().collect();
        let emit = self.operation_callback(request_id, true, refresh_locations);
        let load = provider.rename(
            RenameRequest {
                id: request_id,
                entry,
                new_name,
            },
            emit,
        );
        self.operation_load.replace(Some(load));
    }

    pub fn create_directory(self: &Rc<Self>, parent: Location, name: String) {
        if let Err(message) = validate_basename(&name) {
            self.emit(BrowserEvent::OperationFailed {
                message: message.to_owned(),
            });
            return;
        }
        let Some(provider) = self.operation_provider.borrow().clone() else {
            self.emit(BrowserEvent::OperationFailed {
                message: "File operations are unavailable".to_owned(),
            });
            return;
        };
        let request_id = self.begin_operation();
        let refresh_parent = parent.clone();
        let load = provider.create_directory(
            CreateDirectoryRequest {
                id: request_id,
                parent,
                name,
            },
            self.operation_callback(request_id, false, HashSet::from([refresh_parent])),
        );
        self.operation_load.replace(Some(load));
    }

    pub fn create_file(self: &Rc<Self>, parent: Location, name: String) {
        if let Err(message) = validate_basename(&name) {
            self.emit(BrowserEvent::OperationFailed {
                message: message.to_owned(),
            });
            return;
        }
        let Some(provider) = self.operation_provider.borrow().clone() else {
            self.emit(BrowserEvent::OperationFailed {
                message: "File operations are unavailable".to_owned(),
            });
            return;
        };
        let request_id = self.begin_operation();
        let refresh_parent = parent.clone();
        let load = provider.create_file(
            CreateFileRequest {
                id: request_id,
                parent,
                name,
            },
            self.operation_callback(request_id, false, HashSet::from([refresh_parent])),
        );
        self.operation_load.replace(Some(load));
    }

    pub fn transfer(
        self: &Rc<Self>,
        destination: Location,
        items: Vec<PasteItem>,
        move_sources: bool,
    ) {
        if items.is_empty() {
            return;
        }
        let Some(provider) = self.operation_provider.borrow().clone() else {
            self.emit(BrowserEvent::OperationFailed {
                message: "File operations are unavailable".to_owned(),
            });
            return;
        };
        let request_id = self.begin_operation();
        self.transfer_operation.set(Some(move_sources));
        self.emit(BrowserEvent::TransferStarted {
            total: items.len(),
            moving: move_sources,
        });
        let mut refresh_locations = HashSet::from([destination.clone()]);
        if move_sources {
            for parent in items.iter().filter_map(|item| item.source.parent()) {
                refresh_locations.insert(parent);
            }
        }
        let load = provider.paste(
            PasteRequest {
                id: request_id,
                destination,
                items,
                move_sources,
            },
            self.operation_callback(request_id, false, refresh_locations),
        );
        self.operation_load.replace(Some(load));
    }

    pub fn delete(self: &Rc<Self>, entries: Vec<FileEntry>, permanent: bool) {
        if entries.is_empty() {
            return;
        }
        let Some(provider) = self.operation_provider.borrow().clone() else {
            self.emit(BrowserEvent::OperationFailed {
                message: "File operations are unavailable".to_owned(),
            });
            return;
        };
        let total = entries.len();
        let request_id = self.begin_operation();
        self.deletion_operation.set(true);
        self.deletion_permanent.set(permanent);
        self.emit(BrowserEvent::DeletionStarted { total });
        let load = provider.delete(
            DeleteRequest {
                id: request_id,
                entries,
                permanent,
            },
            self.operation_callback(request_id, false, HashSet::new()),
        );
        self.operation_load.replace(Some(load));
    }

    pub fn restore(self: &Rc<Self>, entries: Vec<FileEntry>) {
        if entries.is_empty() {
            return;
        }
        let Some(provider) = self.operation_provider.borrow().clone() else {
            self.emit(BrowserEvent::OperationFailed {
                message: "File operations are unavailable".to_owned(),
            });
            return;
        };
        let total = entries.len();
        let request_id = self.begin_operation();
        self.restoration_operation.set(true);
        self.emit(BrowserEvent::RestorationStarted { total });
        let load = provider.restore(
            RestoreRequest {
                id: request_id,
                source: RestoreSource::TrashEntries(entries),
            },
            self.operation_callback(request_id, false, HashSet::new()),
        );
        self.operation_load.replace(Some(load));
    }

    pub fn undo_last_trash(self: &Rc<Self>) -> bool {
        if self.current_operation.get().is_some() {
            return false;
        }
        let Some((generation, locations)) = claim_pending_trash_undo() else {
            return false;
        };
        let Some(provider) = self.operation_provider.borrow().clone() else {
            finish_trash_undo(generation, false);
            return false;
        };
        let total = locations.len();
        let request_id = self.begin_operation();
        self.restoration_operation.set(true);
        self.undo_restoration
            .replace(Some((generation, locations.clone())));
        self.emit(BrowserEvent::RestorationStarted { total });
        let load = provider.restore(
            RestoreRequest {
                id: request_id,
                source: RestoreSource::OriginalLocations(locations),
            },
            self.operation_callback(request_id, false, HashSet::new()),
        );
        self.operation_load.replace(Some(load));
        true
    }

    pub fn compress(
        self: &Rc<Self>,
        entries: Vec<FileEntry>,
        destination: Location,
        archive_name: String,
        conflict: TransferConflict,
        format: ArchiveFormat,
        password: Option<String>,
    ) {
        if entries.is_empty() {
            return;
        }
        let Some(provider) = self.operation_provider.borrow().clone() else {
            self.emit(BrowserEvent::OperationFailed {
                message: "File operations are unavailable".to_owned(),
            });
            return;
        };
        let request_id = self.begin_operation();
        let load = provider.compress(
            CompressRequest {
                id: request_id,
                entries,
                destination,
                archive_name,
                conflict,
                format,
                password,
            },
            self.operation_callback(request_id, false, HashSet::new()),
        );
        self.operation_load.replace(Some(load));
    }

    pub fn extract(
        self: &Rc<Self>,
        entry: FileEntry,
        destination: Location,
        password: Option<String>,
    ) {
        let Some(provider) = self.operation_provider.borrow().clone() else {
            self.emit(BrowserEvent::OperationFailed {
                message: "File operations are unavailable".to_owned(),
            });
            return;
        };
        let request_id = self.begin_operation();
        let load = provider.extract(
            ExtractRequest {
                id: request_id,
                entry,
                destination,
                password,
            },
            self.operation_callback(request_id, false, HashSet::new()),
        );
        self.operation_load.replace(Some(load));
    }

    pub fn cancel_file_operation(&self) {
        if self.transfer_operation.get().is_none()
            && !self.deletion_operation.get()
            && !self.restoration_operation.get()
        {
            let had_operation = self.current_operation.replace(None).is_some();
            self.operation_load.borrow_mut().take();
            if had_operation {
                self.emit(BrowserEvent::ArchiveCompleted {
                    select_name: String::new(),
                });
            }
            return;
        }
        self.operation_load.borrow_mut().take();
    }

    fn begin_operation(&self) -> OperationRequestId {
        self.operation_load.borrow_mut().take();
        if let Some((generation, _)) = self.undo_restoration.take() {
            finish_trash_undo(generation, false);
        }
        self.transfer_operation.set(None);
        self.deletion_operation.set(false);
        self.deletion_permanent.set(false);
        self.restoration_operation.set(false);
        let request_id = OperationRequestId(self.next_request.get());
        self.next_request
            .set(self.next_request.get().saturating_add(1));
        self.current_operation.set(Some(request_id));
        request_id
    }

    fn operation_callback(
        self: &Rc<Self>,
        request_id: OperationRequestId,
        rename: bool,
        refresh_locations: HashSet<Location>,
    ) -> Rc<dyn Fn(OperationEvent)> {
        let weak = Rc::downgrade(self);
        Rc::new(move |event| {
            let Some(browser) = weak.upgrade() else {
                return;
            };
            let event_id = match &event {
                OperationEvent::Renamed { request_id }
                | OperationEvent::Created { request_id }
                | OperationEvent::Pasted { request_id, .. }
                | OperationEvent::TransferFailed { request_id, .. }
                | OperationEvent::TransferProgress { request_id, .. }
                | OperationEvent::DeleteProgress { request_id, .. }
                | OperationEvent::RestoreProgress { request_id, .. }
                | OperationEvent::Deleted { request_id, .. }
                | OperationEvent::CompletedWithErrors { request_id, .. }
                | OperationEvent::Restored { request_id, .. }
                | OperationEvent::RestoreCompletedWithErrors { request_id, .. }
                | OperationEvent::Failed { request_id, .. }
                | OperationEvent::Compressed { request_id, .. }
                | OperationEvent::Extracted { request_id, .. }
                | OperationEvent::ArchiveStarted { request_id, .. }
                | OperationEvent::Cancelled { request_id, .. }
                | OperationEvent::ArchiveProgress { request_id, .. } => *request_id,
            };
            if event_id != request_id || browser.current_operation.get() != Some(event_id) {
                return;
            }
            if let OperationEvent::DeleteProgress {
                completed,
                total,
                deleted_location,
                ..
            } = &event
            {
                if *total <= MAX_INCREMENTAL_OPERATION_UPDATES
                    && let Some(location) = deleted_location
                {
                    browser.remove_deleted_locations(std::slice::from_ref(location));
                }
                browser.emit(BrowserEvent::DeletionProgress {
                    completed: *completed,
                    total: *total,
                });
                return;
            }
            if let OperationEvent::TransferProgress {
                completed, total, ..
            } = &event
            {
                browser.emit(BrowserEvent::TransferProgress {
                    completed: *completed,
                    total: *total,
                });
                return;
            }
            if let OperationEvent::RestoreProgress {
                completed,
                total,
                restored_location,
                ..
            } = &event
            {
                if restored_location.is_some()
                    && let Some((generation, locations)) =
                        browser.undo_restoration.borrow().as_ref()
                    && let Some(location) = completed
                        .checked_sub(1)
                        .and_then(|index| locations.get(index))
                {
                    mark_trash_undo_restored(*generation, location);
                }
                if *total <= MAX_INCREMENTAL_OPERATION_UPDATES
                    && let Some(location) = restored_location
                {
                    browser.remove_deleted_locations(std::slice::from_ref(location));
                }
                browser.emit(BrowserEvent::RestorationProgress {
                    completed: *completed,
                    total: *total,
                });
                return;
            }
            if let OperationEvent::ArchiveStarted { total, .. } = &event {
                browser.emit(BrowserEvent::ArchiveStarted { total: *total });
                return;
            }
            if let OperationEvent::ArchiveProgress {
                completed, total, ..
            } = &event
            {
                browser.emit(BrowserEvent::ArchiveProgress {
                    completed: *completed,
                    total: *total,
                });
                return;
            }
            browser.current_operation.set(None);
            let moving = browser.transfer_operation.replace(None);
            let deleting = browser.deletion_operation.replace(false);
            let deletion_permanent = browser.deletion_permanent.replace(false);
            let restoring = browser.restoration_operation.replace(false);
            if restoring && let Some((generation, _)) = browser.undo_restoration.take() {
                finish_trash_undo(
                    generation,
                    matches!(&event, OperationEvent::Restored { .. }),
                );
            }
            if deleting && !deletion_permanent {
                let locations = match &event {
                    OperationEvent::Deleted { locations, .. } => locations.clone(),
                    OperationEvent::CompletedWithErrors {
                        deleted_locations, ..
                    } => deleted_locations.clone(),
                    OperationEvent::Cancelled { result, .. } => result.completed.clone(),
                    _ => Vec::new(),
                };
                if !locations.is_empty() {
                    replace_pending_trash_undo(locations);
                }
            }
            if moving.is_some() {
                let moved_locations = match &event {
                    OperationEvent::Pasted { locations, .. } if moving == Some(true) => {
                        locations.clone()
                    }
                    OperationEvent::Cancelled { result, .. } if moving == Some(true) => {
                        result.completed.clone()
                    }
                    OperationEvent::TransferFailed {
                        completed_locations,
                        ..
                    } if moving == Some(true) => completed_locations.clone(),
                    _ => Vec::new(),
                };
                browser.emit(BrowserEvent::TransferFinished { moved_locations });
            }
            if deleting {
                browser.emit(BrowserEvent::DeletionFinished);
            }
            if restoring {
                browser.emit(BrowserEvent::RestorationFinished);
            }
            browser.operation_load.borrow_mut().take();
            match event {
                OperationEvent::Failed { message, .. } if rename => {
                    browser.emit(BrowserEvent::RenameFailed { message });
                }
                OperationEvent::Failed { message, .. } => {
                    browser.emit(BrowserEvent::OperationFailed { message });
                }
                OperationEvent::TransferFailed { message, .. } => {
                    for location in &refresh_locations {
                        browser.refresh_columns_at(location);
                    }
                    browser.emit(BrowserEvent::OperationFailed { message });
                }
                OperationEvent::CompletedWithErrors {
                    deleted_locations,
                    retryable_locations,
                    has_non_retryable_failures,
                    message,
                    ..
                } => {
                    browser.remove_deleted_locations(&deleted_locations);
                    browser.emit(BrowserEvent::OperationCompletedWithErrors {
                        message,
                        retryable_locations,
                        has_non_retryable_failures,
                    });
                }
                OperationEvent::Deleted { locations, .. }
                | OperationEvent::Restored { locations, .. } => {
                    browser.remove_deleted_locations(&locations);
                }
                OperationEvent::RestoreCompletedWithErrors {
                    restored_locations,
                    message,
                    ..
                } => {
                    browser.remove_deleted_locations(&restored_locations);
                    browser.emit(BrowserEvent::OperationCompletedWithErrors {
                        message,
                        retryable_locations: Vec::new(),
                        has_non_retryable_failures: true,
                    });
                }
                OperationEvent::Cancelled { result, .. } => {
                    let mut affected_locations = refresh_locations.clone();
                    affected_locations.extend(result.affected_locations);
                    browser.emit(BrowserEvent::OperationCancelled {
                        completed: result.completed.len(),
                        failed: result.failed.len(),
                        not_attempted: result.not_attempted.len(),
                        affected_locations,
                    });
                }
                OperationEvent::Renamed { .. } => {
                    browser.emit(BrowserEvent::RenameCompleted);
                    for location in &refresh_locations {
                        if location.native_path().is_none() {
                            browser.refresh_columns_at(location);
                        }
                    }
                }
                OperationEvent::Compressed { archive_name, .. } => {
                    browser.emit(BrowserEvent::ArchiveCompleted {
                        select_name: archive_name.clone(),
                    });
                }
                OperationEvent::Extracted { first_name, .. } => {
                    browser.emit(BrowserEvent::ArchiveCompleted {
                        select_name: first_name.unwrap_or_default(),
                    });
                }
                OperationEvent::Pasted { .. } => {
                    browser.emit(BrowserEvent::TransferCompleted);
                    for location in &refresh_locations {
                        if location.native_path().is_none() {
                            browser.refresh_columns_at(location);
                        }
                    }
                }
                OperationEvent::Created { .. } => {
                    for location in &refresh_locations {
                        if location.native_path().is_none() {
                            browser.refresh_columns_at(location);
                        }
                    }
                }
                OperationEvent::TransferProgress { .. }
                | OperationEvent::DeleteProgress { .. }
                | OperationEvent::RestoreProgress { .. }
                | OperationEvent::ArchiveStarted { .. }
                | OperationEvent::ArchiveProgress { .. } => {}
            }
        })
    }

    pub fn preview(self: &Rc<Self>, depth: usize, position: usize) {
        let Some(entry) = self.entry_at(depth, position) else {
            return;
        };
        if entry.is_directory() && self.is_open_child(depth, &entry.location) {
            self.close_column(depth + 1);
            return;
        }
        self.select(depth, position);
        if entry.is_directory() {
            self.descend(depth, entry.location);
        } else {
            self.emit(BrowserEvent::PreviewRequested { entry });
        }
    }

    pub fn open_location(&self, location: Location) {
        self.emit(BrowserEvent::OpenRequested { location });
    }

    pub fn request_empty_trash(&self) {
        self.emit(BrowserEvent::EmptyTrashRequested);
    }

    pub fn activate(self: &Rc<Self>, depth: usize, position: usize) {
        if self
            .entry_at(depth, position)
            .is_some_and(|entry| entry.is_directory() && self.is_open_child(depth, &entry.location))
        {
            self.close_column(depth + 1);
            return;
        }
        self.select(depth, position);
        self.activate_focused();
    }

    pub(crate) fn is_open_child(&self, parent_depth: usize, location: &Location) -> bool {
        parent_depth
            .checked_add(1)
            .and_then(|depth| self.location_at(depth))
            .as_ref()
            == Some(location)
    }

    /// Activates an item using conventional single-pane explorer navigation.
    pub fn activate_in_place(self: &Rc<Self>, depth: usize, position: usize) {
        self.select(depth, position);
        let Some(entry) = self.entry_at(depth, position) else {
            return;
        };
        if entry.is_directory() {
            self.navigate(entry.location);
        } else {
            self.emit(BrowserEvent::OpenRequested {
                location: entry.location,
            });
        }
    }

    pub fn activate_focused_in_place(self: &Rc<Self>) {
        let Some((depth, position, _)) = self.focused_item() else {
            self.move_selection(1);
            return;
        };
        self.activate_in_place(depth, position);
    }

    pub fn move_selection(&self, direction: i32) {
        let moved = self.state.borrow_mut().move_selection(direction);
        if let Some((depth, position)) = moved {
            self.emit(BrowserEvent::FocusChanged {
                depth,
                position: Some(position),
            });
        }
    }

    pub fn extend_selection(&self, direction: i32) {
        let extended = self.state.borrow_mut().extend_selection(direction);
        if let Some((depth, focused, positions)) = extended {
            self.emit(BrowserEvent::SelectionSetChanged {
                depth,
                positions,
                focused,
                take_focus: true,
            });
        }
    }

    pub fn focus_parent(&self) {
        let focus = self.state.borrow_mut().focus_parent();
        if let Some((depth, position)) = focus {
            self.emit(BrowserEvent::FocusChanged { depth, position });
        }
    }

    fn focus_child(&self) {
        let focus = self.state.borrow_mut().focus_child();
        if let Some((depth, position)) = focus {
            self.emit(BrowserEvent::FocusChanged { depth, position });
        }
    }

    pub fn activate_focused(self: &Rc<Self>) {
        let focused = self.state.borrow().focused_entry();
        let Some((depth, _, entry)) = focused else {
            self.move_selection(1);
            return;
        };

        if entry.is_directory() {
            if self.is_open_child(depth, &entry.location) {
                self.focus_child();
            } else {
                self.descend_with_selection(depth, entry.location, true);
            }
        } else {
            self.emit(BrowserEvent::OpenRequested {
                location: entry.location,
            });
        }
    }

    fn restore_path(self: &Rc<Self>, path: NavigationPath) {
        self.close_peek();
        self.loads.borrow_mut().clear();
        self.monitors.borrow_mut().clear();
        self.cancel_deferred_work();
        let loads: Vec<_> = path
            .locations()
            .iter()
            .cloned()
            .map(|location| {
                let request_id = self.new_request_id();
                (location, request_id)
            })
            .collect();
        self.state
            .borrow_mut()
            .restore(path, loads.iter().map(|(_, request_id)| *request_id));

        self.emit(BrowserEvent::Reset);
        let active_depth = loads.len().checked_sub(1);
        for (depth, (location, request_id)) in loads.into_iter().enumerate() {
            self.emit(BrowserEvent::ColumnAdded {
                depth,
                location: location.clone(),
            });
            self.start_load(depth, location, request_id);
        }
        if let Some(depth) = active_depth {
            self.emit(BrowserEvent::FocusChanged {
                depth,
                position: None,
            });
        }
    }

    fn start_load(self: &Rc<Self>, depth: usize, location: Location, request_id: RequestId) {
        let handle = self.request_directory(depth, location.clone(), request_id);
        self.loads.borrow_mut().push(handle);

        let monitor = self.install_monitor(depth, location);
        self.monitors.borrow_mut().push(monitor);
    }

    fn install_monitor(self: &Rc<Self>, depth: usize, location: Location) -> Option<LoadHandle> {
        let weak: Weak<Self> = Rc::downgrade(self);
        let watched = location.clone();
        let notify = Rc::new(move |change| {
            if let Some(browser) = weak.upgrade() {
                browser.handle_directory_change(depth, &watched, change);
            }
        });
        self.source
            .watch(location, self.preferences.get().show_hidden, notify)
    }

    /// Merges one wire batch (or one coalesced group) and emits its UI
    /// events: the single choke point behind immediate, coalesced, and
    /// straggler application alike.
    fn apply_owned_batch(self: &Rc<Self>, request_id: RequestId, entries: Vec<FileEntry>) {
        let install_started = std::time::Instant::now();
        let mut state = self.state.borrow_mut();
        let batch_len = entries.len();
        let Some((depth, insertions)) = state.apply_batch(request_id, entries) else {
            // The load went away between queueing and flush.
            return;
        };
        tracing::debug!(
            request_id = request_id.0,
            location = %state.columns[depth].location.diagnostic_path(),
            entries = batch_len,
            "directory batch accepted"
        );
        let selected = state.columns[depth].selected;
        drop(state);
        crate::metrics::record_stage(
            "state-install",
            install_started.elapsed().as_millis() as u64,
        );
        self.emit(BrowserEvent::EntriesInserted { depth, insertions });
        // The full-column scan below is the most expensive per-batch work
        // after the merge itself; skip it entirely when nothing is selected.
        if let Some(focused) = selected {
            let positions = self.state.borrow().selected_positions(depth);
            let current = (request_id, positions.clone(), focused);
            let mut last = self.last_batch_selection.borrow_mut();
            if last.get(&depth) != Some(&current) {
                last.insert(depth, current);
                drop(last);
                self.emit(BrowserEvent::SelectionSetChanged {
                    depth,
                    positions,
                    focused,
                    take_focus: false,
                });
            }
        }
    }

    /// Stages one native wire batch: appended to the depth's staging load
    /// with no merge walk and no UI events. Sorting, installation, and
    /// publication all wait for enumeration to finish.
    fn stage_batch(self: &Rc<Self>, request_id: RequestId, depth: usize, entries: Vec<FileEntry>) {
        let mut staging = self.staging.borrow_mut();
        let slot = staging.entry(depth).or_insert_with(|| StagingLoad {
            request_id,
            entries: Vec::new(),
            removed: HashSet::new(),
            deltas: Vec::new(),
        });
        if slot.request_id != request_id {
            // A reload replaced the load mid-stream: the old staging
            // belongs to a discarded load, so restart it instead of mixing
            // generations.
            *slot = StagingLoad {
                request_id,
                entries,
                removed: HashSet::new(),
                deltas: Vec::new(),
            };
            return;
        }
        if slot.entries.is_empty() {
            slot.entries = entries;
        } else {
            slot.entries.extend(entries);
        }
    }

    fn accumulate_batch(
        self: &Rc<Self>,
        request_id: RequestId,
        depth: usize,
        entries: Vec<FileEntry>,
    ) {
        // Remote loads only: a 50 ms timer bounds first-result latency so a
        // slow link never waits solely for the count threshold.
        let mut pending = self.coalesce_pending.borrow_mut();
        let slot = pending
            .entry(depth)
            .or_insert_with(|| (request_id, Vec::new()));
        if slot.0 != request_id {
            // A reload replaced the load mid-stream: the old accumulation
            // belongs to a discarded load, so drop it instead of mixing
            // generations.
            *slot = (request_id, Vec::new());
        }
        slot.1.extend(entries);
        let full = slot.1.len() >= COALESCE_ENTRIES;
        drop(pending);
        if full {
            self.flush_coalesced_capped(Some(depth));
        } else {
            self.arm_remote_flush_timer();
        }
    }

    /// Flushes coalesced remote batches, capped per depth per fire so one
    /// timer fire never becomes a multi-frame GTK mutation. Leftovers stay
    /// queued behind a re-armed timer.
    fn flush_coalesced_capped(self: &Rc<Self>, depth: Option<usize>) {
        let depths: Vec<usize> = match depth {
            Some(depth) => vec![depth],
            None => self.coalesce_pending.borrow().keys().copied().collect(),
        };
        for depth in depths {
            self.drain_publish(depth);
            let chunk: Option<(RequestId, Vec<FileEntry>)> = self
                .coalesce_pending
                .borrow_mut()
                .get_mut(&depth)
                .and_then(|slot| {
                    if slot.1.is_empty() {
                        return None;
                    }
                    let take = slot.1.len().min(REMOTE_FLUSH_CAP);
                    let entries: Vec<FileEntry> = slot.1.drain(..take).collect();
                    Some((slot.0, entries))
                });
            if let Some((request_id, entries)) = chunk {
                self.apply_owned_batch(request_id, entries);
            }
        }
        self.coalesce_pending
            .borrow_mut()
            .retain(|_, (_, entries)| !entries.is_empty());
        if self.coalesce_pending.borrow().is_empty() {
            if let Some(source) = self.remote_flush_timer.borrow_mut().take() {
                source.remove();
            }
        } else {
            self.arm_remote_flush_timer();
        }
    }

    fn arm_remote_flush_timer(self: &Rc<Self>) {
        if self.remote_flush_timer.borrow().is_some() {
            return;
        }
        let weak: Weak<Self> = Rc::downgrade(self);
        let source = gio::glib::timeout_add_local_once(REMOTE_FLUSH_DELAY, move || {
            if let Some(browser) = weak.upgrade() {
                // Spent: disarm before flushing, since the flush disarms an
                // armed timer by removing it (a fired id refuses removal).
                browser.remote_flush_timer.borrow_mut().take();
                browser.flush_coalesced_capped(None);
            }
        });
        *self.remote_flush_timer.borrow_mut() = Some(source);
    }

    /// Sorts a staged native snapshot off the main thread, then installs,
    /// reconciles, and publishes it. The loading state stays up throughout:
    /// no provisional list is ever exposed for a faster first row.
    fn finish_staged_load(self: &Rc<Self>, depth: usize, request_id: RequestId, truncated: bool) {
        let staging = self.staging.borrow_mut().remove(&depth);
        let Some(staging) = staging.filter(|staged| staged.request_id == request_id) else {
            return;
        };
        let preferences = self
            .state
            .borrow()
            .column_preferences(depth)
            .unwrap_or_else(|| self.preferences.get());
        let removed = staging.removed;
        let mut entries = staging.entries;
        entries.retain(|entry| !removed.contains(&entry.location));
        let deltas = staging.deltas;
        self.sorting
            .borrow_mut()
            .insert(depth, SortingLoad { request_id, deltas });
        self.run_sort_task(depth, request_id, entries, preferences, truncated);
    }

    /// Sorts a snapshot inline below the threshold, or in a blocking worker
    /// above it with completion back on the main thread. Small sorts stay
    /// synchronous (no context, no pump); large ones never block input.
    fn run_sort_task(
        self: &Rc<Self>,
        depth: usize,
        request_id: RequestId,
        entries: Vec<FileEntry>,
        preferences: ViewPreferences,
        truncated: bool,
    ) {
        if entries.len() <= SORT_INLINE_LIMIT {
            let sorted = sort_entries(entries, preferences);
            self.finish_staged_sort(depth, request_id, sorted, preferences, truncated);
            return;
        }
        let weak: Weak<Self> = Rc::downgrade(self);
        glib::MainContext::default().spawn_local(async move {
            let sorted = gio::spawn_blocking(move || sort_entries(entries, preferences)).await;
            let Some(browser) = weak.upgrade() else {
                return;
            };
            match sorted {
                Ok(sorted) => {
                    browser.finish_staged_sort(depth, request_id, sorted, preferences, truncated)
                }
                Err(_) => browser.fail_staged_sort(depth, request_id),
            }
        });
    }

    /// Installs a sorted staged snapshot, reconciles monitor deltas queued
    /// while it sorted, and publishes. Drops everything when the load was
    /// superseded mid-sort.
    fn finish_staged_sort(
        self: &Rc<Self>,
        depth: usize,
        request_id: RequestId,
        sorted: Vec<FileEntry>,
        staged_preferences: ViewPreferences,
        truncated: bool,
    ) {
        let sorting = self.sorting.borrow_mut().remove(&depth);
        let Some(sorting) = sorting.filter(|sorting| sorting.request_id == request_id) else {
            return;
        };
        if self.state.borrow().request_id_for_depth(depth) != Some(request_id) {
            return;
        }
        if self
            .state
            .borrow_mut()
            .install_snapshot(request_id, sorted)
            .is_none()
        {
            return;
        }
        // Reconcile silently: the UI model is still empty, so delta events
        // would splice invalid positions. State converges first (in sort
        // order, since reconciled inserts stay sorted), then one staged
        // publication carries the reconciled order.
        for (watched, change) in sorting.deltas {
            if matches!(change, DirectoryChange::Rescan) {
                continue;
            }
            let _applied = self
                .state
                .borrow_mut()
                .apply_directory_change(depth, &watched, change);
        }
        let current = self
            .state
            .borrow()
            .column_preferences(depth)
            .unwrap_or_else(|| self.preferences.get());
        if current != staged_preferences {
            // Resorted mid-load: route through the standard metadata-aware
            // sort path when fields are still missing, else re-sort
            // off-thread with the current preferences. The loading terminal
            // fires exactly once, on whichever path finishes the load.
            if matches!(current.sort_key, SortKey::Size | SortKey::Modified)
                && self.state.borrow().column_unknown_metadata(depth).is_some()
            {
                self.state.borrow_mut().finish(request_id, truncated);
                self.emit(BrowserEvent::LoadFinished { depth, truncated });
                self.ensure_sorted_after_load(depth);
            } else {
                self.resort_installed_column(depth, request_id, current, truncated);
            }
            return;
        }
        let focused = self
            .state
            .borrow()
            .columns
            .get(depth)
            .and_then(|column| column.selected);
        let positions = self.state.borrow().selected_positions(depth);
        let total = self
            .state
            .borrow()
            .columns
            .get(depth)
            .map(|column| column.entries.len())
            .unwrap_or(0);
        self.state.borrow_mut().finish(request_id, truncated);
        self.publish_staged(
            depth,
            request_id,
            total,
            focused,
            positions,
            PublishTerminal::LoadFinished { truncated },
        );
    }

    /// Re-sorts an installed column off-thread after a mid-load preference
    /// change, then publishes the new order through the staged path.
    fn resort_installed_column(
        self: &Rc<Self>,
        depth: usize,
        request_id: RequestId,
        preferences: ViewPreferences,
        truncated: bool,
    ) {
        let Some(entries) = self
            .state
            .borrow()
            .columns
            .get(depth)
            .map(|column| column.entries.clone())
        else {
            return;
        };
        self.sorting.borrow_mut().insert(
            depth,
            SortingLoad {
                request_id,
                deltas: Vec::new(),
            },
        );
        self.run_sort_task(depth, request_id, entries, preferences, truncated);
    }

    /// Fails a staged load whose sort task died: the column keeps its
    /// loading state replaced by an error, exactly like a failed
    /// enumeration, so no spinner hangs.
    fn fail_staged_sort(self: &Rc<Self>, depth: usize, request_id: RequestId) {
        self.sorting.borrow_mut().remove(&depth);
        let mut state = self.state.borrow_mut();
        if state
            .fail(request_id, "Sorting the directory failed.".to_owned())
            .is_some()
        {
            drop(state);
            self.emit(BrowserEvent::LoadFailed {
                depth,
                message: "Sorting the directory failed.".to_owned(),
            });
        }
    }

    /// Publishes an installed column in stages: the first viewport-sized
    /// prefix replaces the model synchronously for fast first correct rows,
    /// then contiguous tails stream from idle callbacks inside a work
    /// budget so no publication callback exceeds a frame. Selection and the
    /// terminal event wait for the final tail.
    fn publish_staged(
        self: &Rc<Self>,
        depth: usize,
        request_id: RequestId,
        total: usize,
        focused: Option<usize>,
        positions: Vec<usize>,
        terminal: PublishTerminal,
    ) {
        self.drain_publish(depth);
        if total <= STAGE_INLINE_LIMIT {
            if self.state.borrow().columns.get(depth).is_none() {
                return;
            }
            self.emit(BrowserEvent::EntriesReplaced {
                depth,
                count: total,
            });
            if let Some(focused) = focused {
                self.emit(BrowserEvent::SelectionSetChanged {
                    depth,
                    positions,
                    focused,
                    take_focus: false,
                });
            }
            self.emit_publish_terminal(depth, terminal);
            return;
        }
        let published = self
            .state
            .borrow()
            .columns
            .get(depth)
            .map_or(0, |column| column.entries.len().min(FIRST_PUBLISH_COUNT));
        self.emit(BrowserEvent::EntriesReplaced {
            depth,
            count: published,
        });
        self.staged_publishes.borrow_mut().insert(
            depth,
            StagedPublish {
                request_id,
                published,
                total,
                focused,
                positions,
                terminal,
            },
        );
        self.arm_publish_timer();
    }

    /// Emits a staged publication's terminal event.
    fn emit_publish_terminal(&self, depth: usize, terminal: PublishTerminal) {
        match terminal {
            PublishTerminal::LoadFinished { truncated } => {
                self.emit(BrowserEvent::LoadFinished { depth, truncated })
            }
            PublishTerminal::SortingFinished => self.emit(BrowserEvent::SortingFinished { depth }),
        }
    }

    /// Completes a staged publication synchronously: emits the remainder,
    /// the deferred selection, and the terminal. Called before any mutation
    /// that assumes the model converged with authoritative state.
    fn drain_publish(self: &Rc<Self>, depth: usize) {
        let staged = self.staged_publishes.borrow_mut().remove(&depth);
        let Some(staged) = staged else {
            return;
        };
        let remainder = self.state.borrow().columns.get(depth).map_or(0, |column| {
            column.entries.len().saturating_sub(staged.published)
        });
        if remainder > 0 {
            self.emit(BrowserEvent::EntriesPublished {
                depth,
                position: staged.published,
                count: remainder,
            });
        }
        if let Some(focused) = staged.focused {
            self.emit(BrowserEvent::SelectionSetChanged {
                depth,
                positions: staged.positions,
                focused,
                take_focus: false,
            });
        }
        self.emit_publish_terminal(depth, staged.terminal);
    }

    /// Drops a staged publication without emitting: the model and state are
    /// both being reset, so nothing is owed.
    fn cancel_publish(&self, depth: usize) {
        self.staged_publishes.borrow_mut().remove(&depth);
        if self.staged_publishes.borrow().is_empty()
            && let Some(source) = self.publish_timer.borrow_mut().take()
        {
            source.remove();
        }
    }

    fn arm_publish_timer(self: &Rc<Self>) {
        if self.publish_timer.borrow().is_some() {
            return;
        }
        let weak: Weak<Self> = Rc::downgrade(self);
        // Idle priority: tails yield to input, paint, and higher-priority
        // sources, streaming rows behind an interactive UI.
        let source = gio::glib::idle_add_local_once(move || {
            if let Some(browser) = weak.upgrade() {
                browser.fire_publish_tails();
            }
        });
        *self.publish_timer.borrow_mut() = Some(source);
    }

    /// Appends one bounded tail chunk per staged depth, then the deferred
    /// selection and terminal for depths that complete. Stops after the
    /// slice budget so sustained publishing never starves the main loop.
    /// Tails whose load was superseded drop without emitting.
    fn fire_publish_tails(self: &Rc<Self>) {
        self.publish_timer.borrow_mut().take();
        let started = std::time::Instant::now();
        loop {
            let depth = self.staged_publishes.borrow().keys().copied().next();
            let Some(depth) = depth else {
                return;
            };
            let current = self
                .staged_publishes
                .borrow()
                .get(&depth)
                .map(|staged| staged.request_id);
            if current.is_some_and(|id| self.state.borrow().request_id_for_depth(depth) != Some(id))
            {
                // Superseded mid-publish: drop without emitting.
                self.staged_publishes.borrow_mut().remove(&depth);
                continue;
            }
            if started.elapsed() >= PUBLISH_SLICE_BUDGET {
                self.arm_publish_timer();
                return;
            }
            let chunk: Option<(usize, usize)> = self
                .staged_publishes
                .borrow()
                .get(&depth)
                .and_then(|staged| {
                    self.state.borrow().columns.get(depth).map(|column| {
                        let end = (staged.published + PUBLISH_TAIL_CHUNK)
                            .min(column.entries.len())
                            .min(staged.total);
                        (staged.published, end.saturating_sub(staged.published))
                    })
                });
            let Some((position, chunk)) = chunk else {
                // The column went away mid-publish: drop the tail.
                self.staged_publishes.borrow_mut().remove(&depth);
                continue;
            };
            if chunk == 0 {
                let staged = self.staged_publishes.borrow_mut().remove(&depth);
                let Some(staged) = staged else {
                    continue;
                };
                if let Some(focused) = staged.focused {
                    self.emit(BrowserEvent::SelectionSetChanged {
                        depth,
                        positions: staged.positions,
                        focused,
                        take_focus: false,
                    });
                }
                self.emit_publish_terminal(depth, staged.terminal);
                continue;
            }
            self.emit(BrowserEvent::EntriesPublished {
                depth,
                position,
                count: chunk,
            });
            if let Some(staged) = self.staged_publishes.borrow_mut().get_mut(&depth) {
                staged.published += chunk;
            }
        }
    }
    pub fn request_metadata_fill(
        self: &Rc<Self>,
        depth: usize,
        position: usize,
        location: Location,
    ) {
        // Ask the provider what it supports instead of rejecting remote
        // locations owner-side: remote and GVfs fills stay on cancellable
        // GIO, and unsupported sources simply answer `Unsupported`.
        if !self.source.supports_metadata_fill(&location) {
            return;
        }
        {
            let mut pending = self.metadata_pending.borrow_mut();
            let queued = pending.entry(depth).or_default();
            if queued.len() < MAX_PENDING_FILL_LOCATIONS
                && !queued.iter().any(|target| target.location == location)
            {
                queued.push(ViewportTarget { position, location });
            }
        }
        // True settle timer: every newly visible row restarts the debounce,
        // so a continuous fling never fires mid-scroll for stale rows.
        if let Some(source) = self.metadata_timer.borrow_mut().take() {
            source.remove();
        }
        let weak: Weak<Self> = Rc::downgrade(self);
        let source = gio::glib::timeout_add_local_once(METADATA_FILL_DEBOUNCE, move || {
            if let Some(browser) = weak.upgrade() {
                browser.flush_metadata_fills();
            }
        });
        *self.metadata_timer.borrow_mut() = Some(source);
    }

    /// Stats a whole column for a size/date sort behind the sort spinner,
    /// then sorts once the pass lands.
    fn request_sort_fill(
        self: &Rc<Self>,
        depth: usize,
        generation: u64,
        preferences: ViewPreferences,
        targets: Vec<(usize, Location)>,
    ) {
        let Some(request_id) = self.state.borrow().request_id_for_depth(depth) else {
            self.pending_sort.set(None);
            self.emit(BrowserEvent::SortingFinished { depth });
            return;
        };
        self.sort_awaiting_fill
            .borrow_mut()
            .replace((generation, depth, request_id, preferences));
        let weak: Weak<Self> = Rc::downgrade(self);
        let emit = Rc::new(move |event| {
            if let Some(browser) = weak.upgrade() {
                browser.handle_directory_event(event);
            }
        });
        let handle = self.source.fill_metadata(
            MetadataRequest {
                id: request_id,
                entries: targets.into_iter().map(|(_, location)| location).collect(),
                full: true,
                time_budget: DIRECTORY_LOAD_TIME_BUDGET,
            },
            emit,
        );
        self.sort_loads.borrow_mut().insert(depth, handle);
    }

    fn finish_awaited_sort(
        self: &Rc<Self>,
        depth: usize,
        generation: u64,
        preferences: ViewPreferences,
    ) {
        self.sort_awaiting_fill.borrow_mut().take();
        self.sort_loads.borrow_mut().remove(&depth);
        let outcome = {
            let mut state = self.state.borrow_mut();
            if self.pending_sort.get() != Some((generation, depth)) {
                return;
            }
            let outcome = state.apply_sort_preferences(depth, preferences);
            self.preferences.set(preferences);
            self.pending_sort.set(None);
            outcome.map(|(focused, positions)| {
                let request_id = state.request_id_for_depth(depth);
                let total = state.columns.get(depth).map(|column| column.entries.len());
                (request_id, total, focused, positions)
            })
        };
        self.notify_preferences_observers();
        match outcome {
            Some((Some(request_id), Some(total), focused, positions)) => {
                self.publish_staged(
                    depth,
                    request_id,
                    total,
                    focused,
                    positions,
                    PublishTerminal::SortingFinished,
                );
            }
            _ => {
                self.emit(BrowserEvent::SortingFinished { depth });
            }
        }
    }
    /// `Complete`; any other outcome abandons the sort without reordering so
    /// a partial pass is never published as correct. Viewport fills need no
    /// outcome handling beyond dropping their handle: unfilled rows keep
    /// their placeholders and retry on their next bind.
    fn handle_metadata_finished(self: &Rc<Self>, request_id: RequestId, outcome: MetadataOutcome) {
        let awaiting = *self.sort_awaiting_fill.borrow();
        if let Some((generation, depth, fill_request, preferences)) = awaiting
            && fill_request == request_id
        {
            self.sort_loads.borrow_mut().remove(&depth);
            if outcome == MetadataOutcome::Complete
                && self.pending_sort.get() == Some((generation, depth))
            {
                self.finish_awaited_sort(depth, generation, preferences);
            } else {
                self.abandon_awaited_sort(depth, generation, outcome);
            }
            return;
        }
        // A viewport fill (or a superseded sort fill) ending: drop its
        // handle. A superseded sort whose column reloaded already had its
        // indicator closed by the reload path; belt-and-braces abandon here
        // in case the reload raced the terminal.
        if let Some(depth) = self.state.borrow().depth_for_request(request_id) {
            self.metadata_loads.borrow_mut().remove(&depth);
        }
        if let Some((generation, depth, _, _)) = awaiting
            && self.state.borrow().depth_for_request(request_id).is_none()
            && self.pending_sort.get() == Some((generation, depth))
        {
            self.abandon_awaited_sort(depth, generation, outcome);
        }
    }

    /// Abandons a waiting sort after a non-complete fill: the prior correct
    /// order is preserved, the indicator stops, and the failure is logged so
    /// a re-sort retry starts from clean state. Every `SortingStarted` still
    /// pairs with exactly one `SortingFinished`.
    fn abandon_awaited_sort(&self, depth: usize, generation: u64, outcome: MetadataOutcome) {
        self.sort_awaiting_fill.borrow_mut().take();
        self.sort_loads.borrow_mut().remove(&depth);
        if self.pending_sort.get() != Some((generation, depth)) {
            return;
        }
        self.pending_sort.set(None);
        tracing::warn!(
            depth,
            generation,
            ?outcome,
            "metadata sort abandoned; prior order preserved"
        );
        self.emit(BrowserEvent::SortingFinished { depth });
    }
    fn cancel_pending_sort_for(&self, depth: usize) {
        let awaiting = *self.sort_awaiting_fill.borrow();
        if let Some((generation, awaiting_depth, _, _)) = awaiting
            && awaiting_depth == depth
        {
            self.abandon_awaited_sort(depth, generation, MetadataOutcome::Cancelled);
            return;
        }
        self.sort_loads.borrow_mut().remove(&depth);
        if self
            .pending_sort
            .get()
            .is_some_and(|(_, pending_depth)| pending_depth == depth)
        {
            // Sort debounce armed but its fill never started: no provider
            // work to cancel, just close the indicator.
            self.pending_sort.set(None);
            self.emit(BrowserEvent::SortingFinished { depth });
        }
    }

    /// Drops everything deferred for columns at or beyond `len`: viewport
    /// truncation, navigation, reload, and close path.
    fn truncate_deferred_from(self: &Rc<Self>, len: usize) {
        if let Some(source) = self.metadata_timer.borrow_mut().take() {
            source.remove();
        }
        self.metadata_pending
            .borrow_mut()
            .retain(|depth, _| *depth < len);
        // Re-arm the settle timer when younger depths still queue fills.
        if !self.metadata_pending.borrow().is_empty() {
            let weak: Weak<Self> = Rc::downgrade(self);
            let source = gio::glib::timeout_add_local_once(METADATA_FILL_DEBOUNCE, move || {
                if let Some(browser) = weak.upgrade() {
                    browser.flush_metadata_fills();
                }
            });
            *self.metadata_timer.borrow_mut() = Some(source);
        }
        self.metadata_loads
            .borrow_mut()
            .retain(|depth, _| *depth < len);
        let state = self.state.borrow();
        self.fill_tokens.borrow_mut().retain(|request_id, _| {
            state
                .depth_for_request(*request_id)
                .is_some_and(|depth| depth < len)
        });
        let awaiting = *self.sort_awaiting_fill.borrow();
        if let Some((generation, depth, _, _)) = awaiting
            && depth >= len
        {
            self.abandon_awaited_sort(depth, generation, MetadataOutcome::Cancelled);
        } else {
            self.sort_loads.borrow_mut().retain(|depth, _| *depth < len);
        }
        self.coalesce_pending
            .borrow_mut()
            .retain(|depth, _| *depth < len);
        self.last_batch_selection
            .borrow_mut()
            .retain(|depth, _| *depth < len);
        // Staged loads and sorts die with their columns; staged publishes
        // cancel without emitting, since both model and state reset.
        self.staging.borrow_mut().retain(|depth, _| *depth < len);
        self.sorting.borrow_mut().retain(|depth, _| *depth < len);
        self.staged_publishes
            .borrow_mut()
            .retain(|depth, _| *depth < len);
        if self.staged_publishes.borrow().is_empty()
            && let Some(source) = self.publish_timer.borrow_mut().take()
        {
            source.remove();
        }
    }

    /// Re-sorts a freshly loaded column whose sort key needs metadata the
    /// streaming enumeration skipped. Name and type sorts never land here.
    fn ensure_sorted_after_load(self: &Rc<Self>, depth: usize) {
        let (needs, preferences) = {
            let state = self.state.borrow();
            let Some(preferences) = state.column_preferences(depth) else {
                return;
            };
            let needs = matches!(preferences.sort_key, SortKey::Size | SortKey::Modified)
                && state.column_unknown_metadata(depth).is_some();
            (needs, preferences)
        };
        if !needs {
            return;
        }
        let generation = self
            .pending_sort
            .get()
            .map_or(1, |(generation, _)| generation.saturating_add(1));
        if let Some((_, previous_depth)) = self.pending_sort.replace(Some((generation, depth))) {
            self.emit(BrowserEvent::SortingFinished {
                depth: previous_depth,
            });
        }
        self.emit(BrowserEvent::SortingStarted { depth });
        let targets = self
            .state
            .borrow()
            .column_unknown_metadata(depth)
            .unwrap_or_default();
        self.request_sort_fill(depth, generation, preferences, targets);
    }

    fn flush_metadata_fills(self: &Rc<Self>) {
        self.metadata_timer.borrow_mut().take();
        let pending: Vec<(usize, Vec<ViewportTarget>)> =
            self.metadata_pending.borrow_mut().drain().collect();
        for (depth, targets) in pending {
            // Refresh the load identity: the column may have reloaded while
            // these rows queued, and a superseded fill must not apply.
            let Some(request_id) = self.state.borrow().request_id_for_depth(depth) else {
                continue;
            };
            let weak: Weak<Self> = Rc::downgrade(self);
            let emit = Rc::new(move |event| {
                if let Some(browser) = weak.upgrade() {
                    browser.handle_directory_event(event);
                }
            });
            let tokens: Vec<(usize, Location)> = targets
                .iter()
                .map(|target| (target.position, target.location.clone()))
                .collect();
            // Stored before the provider runs: synchronous fills answer
            // inside the call, and their chunks join against these tokens.
            self.fill_tokens.borrow_mut().insert(request_id, tokens);
            let handle = self.source.fill_metadata(
                MetadataRequest {
                    id: request_id,
                    entries: targets.into_iter().map(|target| target.location).collect(),
                    full: false,
                    time_budget: METADATA_FILL_TIME_BUDGET,
                },
                emit,
            );
            self.metadata_loads.borrow_mut().insert(depth, handle);
        }
    }

    /// Drops everything a discarded load queued: metadata fills and
    /// coalesced batches alike. Coalesced rows are safe to drop because
    /// every site that clears loads replaces the data source wholesale.
    /// A pending sort's indicator closes here: its fill handle is dropped,
    /// which aborts provider work without a terminal event.
    fn cancel_deferred_work(&self) {
        if let Some(source) = self.metadata_timer.borrow_mut().take() {
            source.remove();
        }
        self.metadata_pending.borrow_mut().clear();
        self.metadata_loads.borrow_mut().clear();
        self.fill_tokens.borrow_mut().clear();
        let awaiting = self.sort_awaiting_fill.borrow_mut().take();
        if let Some((generation, depth, _, _)) = awaiting {
            self.abandon_awaited_sort(depth, generation, MetadataOutcome::Cancelled);
        } else {
            self.sort_loads.borrow_mut().clear();
            if let Some((_, depth)) = self.pending_sort.take() {
                // Sort debounce armed but its fill never started.
                self.emit(BrowserEvent::SortingFinished { depth });
            }
        }
        self.coalesce_pending.borrow_mut().clear();
        self.last_batch_selection.borrow_mut().clear();
        // Staged snapshots, in-flight sorts, and staged publications die
        // with their loads: late completions find no staging entry and a
        // retired request id, so they publish nothing.
        self.staging.borrow_mut().clear();
        self.sorting.borrow_mut().clear();
        self.staged_publishes.borrow_mut().clear();
        if let Some(source) = self.publish_timer.borrow_mut().take() {
            source.remove();
        }
        if let Some(source) = self.remote_flush_timer.borrow_mut().take() {
            source.remove();
        }
    }

    fn request_directory(
        self: &Rc<Self>,
        depth: usize,
        location: Location,
        request_id: RequestId,
    ) -> LoadHandle {
        let weak: Weak<Self> = Rc::downgrade(self);
        let emit = Rc::new(move |event| {
            if let Some(browser) = weak.upgrade() {
                browser.handle_directory_event(event);
            }
        });
        let batch_size = if location.native_path().is_some() {
            NATIVE_DIRECTORY_BATCH_SIZE
        } else {
            REMOTE_DIRECTORY_BATCH_SIZE
        };
        // Loads sorted by size or date stat inline: the column's own key
        // decides, falling back to the application preference for columns
        // that do not exist yet. Sorting placeholders and re-sorting a full
        // directory afterwards costs more than one stat per file up front.
        let sort_key = self
            .state
            .borrow()
            .column_preferences(depth)
            .map(|preferences| preferences.sort_key)
            .unwrap_or_else(|| self.preferences.get().sort_key);
        let include_metadata = matches!(sort_key, SortKey::Size | SortKey::Modified);
        self.source.enumerate(
            DirectoryRequest {
                id: request_id,
                location,
                batch_size,
                include_metadata,
                max_entries: MAX_DIRECTORY_ENTRIES,
                time_budget: DIRECTORY_LOAD_TIME_BUDGET,
            },
            emit,
        )
    }

    pub(crate) fn refresh_columns_at(self: &Rc<Self>, location: &Location) {
        let depths = {
            let state = self.state.borrow();
            let mut depths = Vec::new();
            let mut depth = 0;
            while let Some(open_location) = state.location_at(depth) {
                if &open_location == location {
                    depths.push(depth);
                }
                depth += 1;
            }
            depths
        };
        for depth in depths {
            self.refresh_column(depth);
        }
    }

    pub(crate) fn refresh_after_cancellation(self: &Rc<Self>, roots: &HashSet<Location>) {
        self.refresh_columns_at_or_below(roots);
    }

    fn refresh_columns_at_or_below(self: &Rc<Self>, roots: &HashSet<Location>) {
        let open_locations = {
            let state = self.state.borrow();
            let mut locations = Vec::new();
            let mut depth = 0;
            while let Some(location) = state.location_at(depth) {
                locations.push((depth, location));
                depth += 1;
            }
            locations
        };
        for (depth, location) in open_locations {
            if location_or_ancestor_is_affected(&location, roots) {
                self.refresh_column(depth);
            }
        }
    }

    fn remove_deleted_locations(self: &Rc<Self>, locations: &[Location]) {
        if locations.len() > MAX_INCREMENTAL_OPERATION_UPDATES {
            let parents: HashSet<_> = locations
                .iter()
                .filter_map(deletion_parent_location)
                .collect();
            for parent in parents {
                self.refresh_columns_at(&parent);
            }
            return;
        }
        for location in locations {
            let Some(parent) = deletion_parent_location(location) else {
                continue;
            };
            let depths = {
                let state = self.state.borrow();
                let mut depths = Vec::new();
                let mut depth = 0;
                while let Some(open_location) = state.location_at(depth) {
                    if open_location == parent {
                        depths.push(depth);
                    }
                    depth += 1;
                }
                depths
            };
            for depth in depths {
                self.handle_directory_change(
                    depth,
                    &parent,
                    DirectoryChange::Remove(location.clone()),
                );
            }
        }
    }

    pub fn retry_column(self: &Rc<Self>, depth: usize) {
        self.refresh_column(depth);
    }

    fn refresh_column(self: &Rc<Self>, depth: usize) {
        let request_id = self.new_request_id();
        let location = self.state.borrow_mut().reload_column(depth, request_id);
        let Some(location) = location else {
            return;
        };
        self.emit(BrowserEvent::ColumnReloaded { depth });
        let handle = self.request_directory(depth, location, request_id);
        if let Some(load) = self.loads.borrow_mut().get_mut(depth) {
            *load = handle;
        }
        self.metadata_loads.borrow_mut().remove(&depth);
        self.metadata_pending.borrow_mut().remove(&depth);
        self.coalesce_pending.borrow_mut().remove(&depth);
        self.last_batch_selection.borrow_mut().remove(&depth);
        self.cancel_pending_sort_for(depth);
        // The retired load's staging, sort, and publication die with it;
        // late completions find a retired request id and publish nothing.
        self.staging.borrow_mut().remove(&depth);
        self.sorting.borrow_mut().remove(&depth);
        self.cancel_publish(depth);
        // Tokens belong to live loads: the retired request id no longer
        // resolves, so its tokens drop here instead of leaking.
        self.fill_tokens
            .borrow_mut()
            .retain(|request_id, _| self.state.borrow().depth_for_request(*request_id).is_some());
    }

    pub fn reload_active(self: &Rc<Self>) {
        if let Some(depth) = self.active_depth() {
            self.refresh_column(depth);
        }
    }

    pub fn refresh_all(self: &Rc<Self>) {
        let depths: Vec<usize> = {
            let state = self.state.borrow();
            (0..state.columns.len()).collect()
        };
        if depths.is_empty() {
            self.reload_active();
            return;
        }
        for depth in depths {
            self.refresh_column(depth);
        }
    }

    pub fn select_entries_by_name(self: &Rc<Self>, names: &[String]) {
        let Some(depth) = self.active_depth() else {
            return;
        };
        let requested: HashSet<&str> = names.iter().map(String::as_str).collect();
        let state = self.state.borrow();
        let Some(column) = state.columns.get(depth) else {
            return;
        };
        let positions: Vec<usize> = column
            .entries
            .iter()
            .enumerate()
            .filter_map(|(position, entry)| {
                requested
                    .contains(entry.display_name.as_str())
                    .then_some(position)
            })
            .collect();
        drop(state);
        let Some(&focused) = positions.last() else {
            return;
        };
        self.set_selection(depth, &positions, Some(focused));
        self.emit(BrowserEvent::SelectionSetChanged {
            depth,
            positions,
            focused,
            take_focus: true,
        });
    }

    /// Depth and nativeness of a column load, if `request_id` still owns
    /// one. Nativeness splits publication policy: native loads stage and
    /// sort once, remote loads stream progressively.
    fn load_target(&self, request_id: RequestId) -> Option<(usize, bool)> {
        let state = self.state.borrow();
        let depth = state.depth_for_request(request_id)?;
        let native = state.location_at(depth)?.native_path().is_some();
        Some((depth, native))
    }

    fn handle_directory_change(
        self: &Rc<Self>,
        depth: usize,
        watched: &Location,
        change: DirectoryChange,
    ) {
        if matches!(&change, DirectoryChange::Rescan) {
            self.refresh_column(depth);
            return;
        }
        // A staged or sorting load owns no published rows yet: queue the
        // delta for the completion's single reconcile instead of racing the
        // snapshot. Removed locations also filter staged batches so a late
        // batch never resurrects them; an upserted location leaves the set
        // so recreations still land.
        if let Some(staging) = self.staging.borrow_mut().get_mut(&depth) {
            match &change {
                DirectoryChange::Remove(location) => {
                    staging.removed.insert(location.clone());
                }
                DirectoryChange::Upsert(entry) => {
                    staging.removed.remove(&entry.location);
                }
                DirectoryChange::Move { from, entry } => {
                    staging.removed.insert(from.clone());
                    staging.removed.remove(&entry.location);
                }
                DirectoryChange::Rescan => {}
            }
            staging.deltas.push((watched.clone(), change));
            return;
        }
        if let Some(sorting) = self.sorting.borrow_mut().get_mut(&depth) {
            sorting.deltas.push((watched.clone(), change));
            return;
        }
        // A staged publication covers a converged model again first: deltas
        // splice positions that only exist past the tails.
        self.drain_publish(depth);
        let path_update = self
            .state
            .borrow()
            .path_after_external_change(depth, &change);
        if let Some(path) = path_update {
            self.restore_path(path);
            return;
        }
        let application = self
            .state
            .borrow_mut()
            .apply_directory_change(depth, watched, change);
        if let Some((splices, selected)) = application {
            let positions = self.state.borrow().selected_positions(depth);
            self.emit(BrowserEvent::EntriesSpliced {
                depth,
                splices,
                selected,
            });
            if let Some(focused) = selected {
                self.emit(BrowserEvent::SelectionSetChanged {
                    depth,
                    positions,
                    focused,
                    take_focus: false,
                });
            }
            self.emit(BrowserEvent::FocusChanged {
                depth,
                position: selected,
            });
        }
    }

    fn handle_directory_event(self: &Rc<Self>, event: DirectoryEvent) {
        match event {
            DirectoryEvent::Batch {
                request_id,
                entries,
            } => {
                // Native open loads stage identity batches with no merge
                // walk and no UI events; everything else stays progressive
                // (remote first paint, peek batches, stragglers).
                let target = self.load_target(request_id);
                let open = self.state.borrow().open_load_depth(request_id);
                match (target, open) {
                    (Some((depth, true)), Some(_)) => {
                        self.stage_batch(request_id, depth, entries);
                    }
                    (Some((depth, false)), Some(_)) => {
                        let entry_count = self
                            .state
                            .borrow()
                            .loading_column(request_id)
                            .map(|(_, count)| count)
                            .unwrap_or(0);
                        if entry_count == 0 {
                            self.apply_owned_batch(request_id, entries);
                        } else {
                            self.accumulate_batch(request_id, depth, entries);
                        }
                    }
                    _ => {
                        let peek_entries: Vec<_> = if self.preferences.get().show_hidden {
                            entries
                        } else {
                            entries
                                .into_iter()
                                .filter(|entry| !entry.is_hidden)
                                .collect()
                        };
                        let mut state = self.state.borrow_mut();
                        if state.apply_peek_batch(request_id, &peek_entries) {
                            drop(state);
                            self.emit(BrowserEvent::PeekEntriesAdded {
                                entries: peek_entries,
                            });
                        }
                    }
                }
            }

            DirectoryEvent::Finished {
                request_id,
                truncated,
            } => {
                // A staged native load sorts off-thread and publishes
                // staged; everything else lands coalesced rows first, then
                // closes the load. Bound to a variable first: an if-let
                // scrutinee borrow would stay live across the flush and
                // panic inside it.
                let target = self.load_target(request_id);
                let open = self.state.borrow().open_load_depth(request_id);
                match (target, open) {
                    (Some((depth, true)), Some(_)) => {
                        self.stage_batch(request_id, depth, Vec::new());
                        self.finish_staged_load(depth, request_id, truncated);
                    }
                    (Some((depth, _)), Some(_)) => {
                        self.flush_coalesced_capped(Some(depth));
                        let mut state = self.state.borrow_mut();
                        if let Some(depth) = state.finish(request_id, truncated) {
                            drop(state);
                            self.emit(BrowserEvent::LoadFinished { depth, truncated });
                            // A column sorted by size or date loaded
                            // placeholders; stat the column and sort once
                            // the pass lands.
                            self.ensure_sorted_after_load(depth);
                        }
                    }
                    _ => {
                        let mut state = self.state.borrow_mut();
                        if state.finish_peek(request_id) {
                            drop(state);
                            self.emit(BrowserEvent::PeekFinished);
                        }
                    }
                }
            }
            DirectoryEvent::Failed {
                request_id,
                message,
            } => {
                // Staged and sorting loads die with the enumeration: drop
                // their state so no late sort can publish, then follow the
                // standard failure path.
                let target = self.load_target(request_id);
                let open = self.state.borrow().open_load_depth(request_id);
                if let Some((depth, true)) = target.filter(|_| open.is_some()) {
                    self.staging.borrow_mut().remove(&depth);
                    self.sorting.borrow_mut().remove(&depth);
                    self.cancel_publish(depth);
                } else if let Some((depth, _)) = target {
                    self.flush_coalesced_capped(Some(depth));
                }
                let mut state = self.state.borrow_mut();
                if let Some(depth) = state.fail(request_id, message.clone()) {
                    drop(state);
                    self.emit(BrowserEvent::LoadFailed { depth, message });
                } else if state.fail_peek(request_id, message.clone()) {
                    drop(state);
                    self.emit(BrowserEvent::PeekFailed { message });
                }
            }
            DirectoryEvent::MetadataFilled {
                request_id,
                updates,
            } => {
                // Full sort fills apply by location: the column is about to
                // be re-sorted wholesale, so positional tokens would only add
                // validation churn to an already O(n log n) path.
                let awaiting_sort = self
                    .sort_awaiting_fill
                    .borrow()
                    .is_some_and(|(_, _, fill_id, _)| fill_id == request_id);
                if awaiting_sort {
                    let mut state = self.state.borrow_mut();
                    if let Some((depth, positions)) = state.apply_metadata(request_id, updates) {
                        let filled = filled_entries(&state, depth, &positions);
                        tracing::debug!(
                            request_id = request_id.0,
                            depth,
                            filled = positions.len(),
                            "metadata fill applied"
                        );
                        drop(state);
                        self.emit(BrowserEvent::MetadataFilled {
                            depth,
                            updates: filled,
                        });
                    }
                    // Sorts wait for the fill's terminal outcome, never for a
                    // chunk: sorting here would publish a partially statted
                    // column as correctly ordered.
                    return;
                }
                // Viewport fills apply in O(requested rows) against the
                // tokens captured at bind time. Rows that moved under the
                // fill go stale and keep their placeholders; their next bind
                // re-requests them, so only stale rows retry.
                let tokens = self.fill_tokens.borrow().get(&request_id).cloned();
                let Some(tokens) = tokens else {
                    return;
                };
                let token_positions: HashMap<&Location, usize> = tokens
                    .iter()
                    .map(|(position, location)| (location, *position))
                    .collect();
                let mut positioned = Vec::with_capacity(updates.len());
                for update in &updates {
                    if let Some(position) = token_positions.get(&update.location) {
                        positioned.push((*position, update.clone()));
                    }
                }
                let mut state = self.state.borrow_mut();
                if let Some((depth, positions, stale)) =
                    state.apply_positioned_metadata(request_id, positioned)
                {
                    let filled = filled_entries(&state, depth, &positions);
                    tracing::debug!(
                        request_id = request_id.0,
                        depth,
                        filled = positions.len(),
                        stale = stale.len(),
                        "metadata fill applied"
                    );
                    drop(state);
                    if !filled.is_empty() {
                        self.emit(BrowserEvent::MetadataFilled {
                            depth,
                            updates: filled,
                        });
                    }
                }
            }
            DirectoryEvent::MetadataFinished {
                request_id,
                outcome,
            } => {
                self.fill_tokens.borrow_mut().remove(&request_id);
                self.handle_metadata_finished(request_id, outcome);
            }
        }
    }

    fn emit(&self, event: BrowserEvent) {
        let observers = self.observers.borrow().clone();
        for observer in &observers {
            observer(&event);
        }
    }

    fn new_request_id(&self) -> RequestId {
        let id = self.next_request.get();
        self.next_request.set(id.saturating_add(1));
        RequestId(id)
    }
}

/// Clones the freshly filled entries for a view refresh payload.
fn filled_entries(
    state: &NavigationState,
    depth: usize,
    positions: &[usize],
) -> Vec<(usize, FileEntry)> {
    positions
        .iter()
        .filter_map(|position| {
            let entry = state.columns.get(depth)?.entries.get(*position)?.clone();
            Some((*position, entry))
        })
        .collect()
}

fn location_or_ancestor_is_affected(location: &Location, roots: &HashSet<Location>) -> bool {
    let mut current = Some(location.clone());
    while let Some(location) = current {
        if roots.contains(&location) {
            return true;
        }
        current = location.parent();
    }
    false
}

fn deletion_parent_location(location: &Location) -> Option<Location> {
    if location
        .uri_value()
        .is_some_and(|uri| uri.starts_with("trash:"))
    {
        Some(Location::uri("trash:///"))
    } else {
        location.parent()
    }
}

fn location_from_input(input: &str) -> Result<Location, LocationValidationError> {
    location_from_input_with_home(input, &glib::home_dir())
}

fn location_from_input_with_home(
    input: &str,
    home: &Path,
) -> Result<Location, LocationValidationError> {
    if input == "~" {
        return Ok(Location::local(home));
    }
    if let Some(relative) = input.strip_prefix("~/") {
        return Ok(Location::local(home.join(relative.trim_start_matches('/'))));
    }
    if input.starts_with('~') {
        return Err(LocationValidationError::UnsupportedShorthand(
            "Only ~ and ~/ paths are supported for the current user's home directory.".to_owned(),
        ));
    }
    if !is_uri_like(input) {
        return Ok(Location::local(PathBuf::from(input)));
    }
    let scheme_end = input.find("://").unwrap_or_default();
    let scheme = &input[..scheme_end];
    let normalized = scheme.to_ascii_lowercase();
    if !matches!(
        normalized.as_str(),
        "smb" | "sftp" | "ftp" | "ftps" | "dav" | "davs" | "trash" | "network"
    ) {
        return Err(LocationValidationError::UnsupportedScheme(format!(
            "The {scheme}:// scheme isn't supported. Use an absolute local path or one of: \
             smb://, sftp://, ftp://, ftps://, dav://, or davs://."
        )));
    }
    validate_uri_credentials(input)?;
    let uri = format!("{normalized}{}", &input[scheme_end..]);
    Ok(Location::uri(uri))
}

/// UNC paths (`\\host\share`, bare `//host/share`) and SCP-style addresses
/// (`user@host:path`) are deliberately not accepted as location-bar shorthand
/// (see lgse/strata#20) so a proper URI (`smb://`, `sftp://`, ...) is always
/// preserved verbatim rather than being guessed at. Report a clear message
/// instead of silently treating either as a relative local path.
fn unsupported_shorthand_message(input: &str) -> Option<&'static str> {
    let looks_like_unc = input.starts_with("\\\\")
        || ["smb:", "SMB:"].iter().any(|prefix| {
            input
                .strip_prefix(prefix)
                .is_some_and(|rest| rest.starts_with("\\\\"))
        });
    // A bare `//host/share` has no scheme, so it is not a valid URI (unlike
    // `smb://host/share`, which `is_uri_like` already accepts untouched).
    let looks_like_bare_network_shorthand = input.starts_with("//") && !is_uri_like(input);
    if looks_like_unc || looks_like_bare_network_shorthand || looks_like_scp_shorthand(input) {
        Some(
            "UNC paths (\\\\host\\share) and SCP-style addresses (user@host:path) aren't \
             supported. Use a URI instead, such as smb://host/share, sftp://host/path, \
             ftp://host/path, or dav://host/path.",
        )
    } else {
        None
    }
}

fn looks_like_scp_shorthand(input: &str) -> bool {
    if is_uri_like(input) {
        return false;
    }
    let Some((_user, after_at)) = input.split_once('@') else {
        return false;
    };
    let Some(host) = after_at.split(':').next() else {
        return false;
    };
    !host.is_empty() && after_at.contains(':') && !host.contains('/') && !host.contains('\\')
}

fn is_uri_like(input: &str) -> bool {
    let Some(scheme_end) = input.find("://") else {
        return false;
    };
    let scheme = &input[..scheme_end];
    scheme.starts_with(|character: char| character.is_ascii_alphabetic())
        && scheme.chars().all(|character| {
            character.is_ascii_alphanumeric() || matches!(character, '+' | '.' | '-')
        })
}

#[cfg(test)]
mod tests;
