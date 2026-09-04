// SPDX-License-Identifier: GPL-3.0-or-later

use std::{
    cell::RefCell,
    collections::{HashMap, VecDeque},
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    time::{Duration, Instant},
};

use gtk::{gdk, gio, glib, prelude::*};

use crate::{
    model::{FileEntry, MetadataValue},
    sandbox::{Cancellation, ParseOperation},
};

static NEXT_REQUEST: AtomicU64 = AtomicU64::new(1);
const MAX_CACHE_ENTRIES: usize = 256;
const MAX_CACHE_BYTES: usize = 64 * 1024 * 1024;
/// Four concurrent sandbox decodes shorten the visible thumbnail drain;
/// viewport bounding keeps the amount of admitted work fixed.
const MAX_THUMBNAIL_WORKERS: usize = 4;
const MAX_QUEUED_THUMBNAILS: usize = 64;
const FAILED_THUMBNAIL_TTL: Duration = Duration::from_secs(30);
/// Settles scrolling before decoding: binds during a fling only park their
/// rows, and one timer fire starts jobs for the rows still on screen. Cache
/// hits bypass the gate entirely, so revisits never wait on it.
const THUMBNAIL_SETTLE_DELAY: Duration = Duration::from_millis(120);
/// Bounds starvation: continuous binds re-arm the settle timer, but the
/// first park starts the clock and anything parked longer fires anyway. The
/// fire still intersects the viewport, so a mid-fling fire only spends work
/// on rows that are actually visible.
const MAX_SETTLE_WAIT: Duration = Duration::from_millis(400);
/// Viewport overscan fraction per side: visible rows plus roughly 25%
/// prefetch start work; anything further out waits for a later settle.
const VIEWPORT_OVERSCAN: f32 = 0.25;

thread_local! {
    static ACTIVE_REQUESTS: RefCell<HashMap<usize, ActiveRequest>> =
        RefCell::new(HashMap::new());
    static PENDING_THUMBNAILS: RefCell<HashMap<ThumbnailKey, PendingThumbnail>> =
        RefCell::new(HashMap::new());
    static THUMBNAIL_QUEUE: RefCell<ThumbnailQueue> = RefCell::new(ThumbnailQueue::default());
    static THUMBNAIL_CACHE: RefCell<ThumbnailCache> = RefCell::new(ThumbnailCache::default());
    /// Fallback settle group (key zero) for targets with no viewport
    /// ancestor (popups, tests): fires on the same per-group machinery.
    /// One settle group per visible scrolled window, keyed by widget
    /// address. A fling in one view re-arms only its own timer, so windows
    /// can never postpone each other's thumbnails.
    static SETTLE_VIEWS: RefCell<HashMap<usize, ViewSettle>> = RefCell::new(HashMap::new());
    /// Rows parked while their file metadata is still unknown. The next
    /// metadata fill promotes them through `note_metadata` instead of
    /// rendering once without an mtime and again after it arrives.
    static METADATA_WAITERS: RefCell<HashMap<PathBuf, Vec<MetadataWaiter>>> =
        RefCell::new(HashMap::new());
}

struct ActiveRequest {
    id: u64,
    image: glib::WeakRef<gtk::Image>,
    deferred: Option<DeferredThumbnail>,
}

#[derive(Clone)]
struct DeferredThumbnail {
    key: ThumbnailKey,
    kind: ThumbnailKind,
}

struct PendingTarget {
    image_id: usize,
    request: u64,
    image: glib::WeakRef<gtk::Image>,
}
/// One parked row: the render key, its provider, the target widget, and
/// whether the row must wait for file metadata before it may start work.
struct SettledPark {
    key: ThumbnailKey,
    kind: ThumbnailKind,
    target: PendingTarget,
    wait_for_metadata: bool,
}

/// Settle state for one visible scrolled window.
struct ViewSettle {
    viewport: glib::WeakRef<gtk::ScrolledWindow>,
    pending: Vec<SettledPark>,
    timer: Option<glib::SourceId>,
    first_park: Option<Instant>,
    hooked: bool,
}

/// A row parked while its file metadata is unknown. Promoted by
/// `note_metadata` into its original viewport group once the fill arrives.
struct MetadataWaiter {
    group: usize,
    kind: ThumbnailKind,
    target: PendingTarget,
    file_size: Option<u64>,
    thumbnail_size: i32,
}
/// One validated render awaiting disk persistence.
struct PersistJob {
    path: PathBuf,
    mtime: i64,
    png: Vec<u8>,
}

/// Bounded persistence queue. Slow or failing disk must never delay display
/// or hold a render-worker slot, so validated renders land here and a single
/// background pump stores them. Over capacity the oldest entry drops: its
/// file simply re-renders on the next cold start.
const MAX_PERSIST_QUEUE: usize = 32;

struct PersistQueue {
    queue: VecDeque<PersistJob>,
}

impl PersistQueue {
    const fn new() -> Self {
        Self {
            queue: VecDeque::new(),
        }
    }

    fn push(&mut self, job: PersistJob) {
        if self.queue.len() >= MAX_PERSIST_QUEUE {
            self.queue.pop_front();
        }
        self.queue.push_back(job);
    }

    fn pop_front(&mut self) -> Option<PersistJob> {
        self.queue.pop_front()
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.queue.len()
    }
}

// Process-wide: the persistence pump runs on a worker thread, so the queue
// cannot be a main-thread local like the settle state.
static PERSIST_QUEUE: std::sync::Mutex<PersistQueue> = std::sync::Mutex::new(PersistQueue::new());
static PERSIST_RUNNING: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Enqueues a validated render for background persistence and ensures the
/// pump runs. Display already applied; this never blocks the caller.
fn enqueue_persist(path: PathBuf, mtime: i64, png: Vec<u8>) {
    PERSIST_QUEUE
        .lock()
        .unwrap_or_else(|poison| poison.into_inner())
        .push(PersistJob { path, mtime, png });
    pump_persist_queue();
}

fn pump_persist_queue() {
    use std::sync::atomic::Ordering;
    if PERSIST_RUNNING.swap(true, Ordering::SeqCst) {
        return;
    }
    gio::spawn_blocking(|| {
        loop {
            let job = PERSIST_QUEUE
                .lock()
                .unwrap_or_else(|poison| poison.into_inner())
                .pop_front();
            let Some(job) = job else {
                break;
            };
            // Best effort, on no render slot: every store failure is
            // silently dropped and the in-memory result already applied.
            super::thumbnail_cache::store(&job.path, job.mtime, &job.png);
        }
        PERSIST_RUNNING.store(false, Ordering::SeqCst);
        // A job enqueued after the drain but before the flag cleared
        // restarts the pump instead of stranding work.
        if !PERSIST_QUEUE
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .queue
            .is_empty()
        {
            pump_persist_queue();
        }
    });
}

struct PendingThumbnail {
    id: u64,
    kind: ThumbnailKind,
    cancellation: Cancellation,
    targets: Vec<PendingTarget>,
}

struct ThumbnailJob {
    id: u64,
    key: ThumbnailKey,
    kind: ThumbnailKind,
    cancellation: Cancellation,
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct ThumbnailKey {
    path: PathBuf,
    modified: Option<i64>,
    file_size: Option<u64>,
    thumbnail_size: i32,
}

#[derive(Default)]
struct ThumbnailCache {
    entries: HashMap<ThumbnailKey, CachedThumbnail>,
    recent: VecDeque<ThumbnailKey>,
    byte_count: usize,
}

#[derive(Clone)]
enum CachedThumbnail {
    Ready(glib::Bytes),
    Failed(Instant),
}

enum CacheHit {
    Ready(glib::Bytes),
    Failed,
}

impl ThumbnailCache {
    fn get(&mut self, key: &ThumbnailKey) -> Option<CacheHit> {
        let entry = self.entries.get(key)?.clone();
        if matches!(entry, CachedThumbnail::Failed(expires) if expires <= Instant::now()) {
            self.remove(key);
            return None;
        }
        self.recent.retain(|candidate| candidate != key);
        self.recent.push_back(key.clone());
        Some(match entry {
            CachedThumbnail::Ready(bytes) => CacheHit::Ready(bytes),
            CachedThumbnail::Failed(_) => CacheHit::Failed,
        })
    }

    fn insert(&mut self, key: ThumbnailKey, bytes: glib::Bytes) {
        self.insert_entry(key, CachedThumbnail::Ready(bytes));
    }

    fn insert_failure(&mut self, key: ThumbnailKey) {
        self.insert_entry(
            key,
            CachedThumbnail::Failed(Instant::now() + FAILED_THUMBNAIL_TTL),
        );
    }

    fn insert_entry(&mut self, key: ThumbnailKey, entry: CachedThumbnail) {
        self.remove(&key);
        self.byte_count = self.byte_count.saturating_add(entry.byte_len());
        self.recent.push_back(key.clone());
        self.entries.insert(key, entry);
        while self.entries.len() > MAX_CACHE_ENTRIES || self.byte_count > MAX_CACHE_BYTES {
            let Some(oldest) = self.recent.pop_front() else {
                break;
            };
            if let Some(removed) = self.entries.remove(&oldest) {
                self.byte_count = self.byte_count.saturating_sub(removed.byte_len());
            }
        }
    }

    fn remove(&mut self, key: &ThumbnailKey) {
        if let Some(removed) = self.entries.remove(key) {
            self.byte_count = self.byte_count.saturating_sub(removed.byte_len());
        }
        self.recent.retain(|candidate| candidate != key);
    }
}

impl CachedThumbnail {
    fn byte_len(&self) -> usize {
        match self {
            Self::Ready(bytes) => bytes.len(),
            Self::Failed(_) => 0,
        }
    }
}

#[derive(Default)]
struct ThumbnailQueue {
    running: usize,
    queued: VecDeque<ThumbnailKey>,
}

impl ThumbnailQueue {
    fn enqueue(&mut self, key: ThumbnailKey) -> bool {
        if self.queued.len() >= MAX_QUEUED_THUMBNAILS {
            return false;
        }
        self.queued.push_back(key);
        true
    }

    fn begin_next(&mut self) -> Option<ThumbnailKey> {
        if self.running >= MAX_THUMBNAIL_WORKERS {
            return None;
        }
        let key = self.queued.pop_front()?;
        self.running += 1;
        Some(key)
    }

    fn finish(&mut self) {
        self.running = self.running.saturating_sub(1);
    }

    fn cancel(&mut self, key: &ThumbnailKey) {
        self.queued.retain(|queued| queued != key);
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ThumbnailKind {
    Image,
    RawImage,
    Pdf,
    Video,
}

pub(super) fn set_thumbnail_or_icon(
    image: &gtk::Image,
    entry: &FileEntry,
    fallback_icon: &str,
    icon_size: i32,
    thumbnail_size: i32,
) {
    let Some(path) = entry.location.native_path() else {
        show_fallback_icon(image, fallback_icon, icon_size);
        return;
    };
    set_thumbnail_for_path(ThumbnailRequest {
        image,
        path,
        modified: known_metadata(&entry.modified_unix_seconds),
        file_size: known_metadata(&entry.size),
        fallback_icon,
        icon_size,
        thumbnail_size,
        wait_for_metadata: true,
    });
}

pub(super) fn set_thumbnail_or_icon_for_path(
    image: &gtk::Image,
    path: &Path,
    fallback_icon: &str,
    icon_size: i32,
    thumbnail_size: i32,
) {
    set_thumbnail_for_path(ThumbnailRequest {
        image,
        path,
        modified: None,
        file_size: None,
        fallback_icon,
        icon_size,
        thumbnail_size,
        wait_for_metadata: false,
    });
}

/// One thumbnail scheduling request. Bundled so the scheduler entry point
/// stays under the argument-count lint as viewport and metadata flags join
/// the key material.
struct ThumbnailRequest<'a> {
    image: &'a gtk::Image,
    path: &'a Path,
    modified: Option<i64>,
    file_size: Option<u64>,
    fallback_icon: &'a str,
    icon_size: i32,
    thumbnail_size: i32,
    wait_for_metadata: bool,
}

fn set_thumbnail_for_path(request: ThumbnailRequest<'_>) {
    let (image_id, request_id) =
        set_fallback_icon(request.image, request.fallback_icon, request.icon_size);
    let path = request.path.to_path_buf();
    let Some(kind) = thumbnail_kind(&path) else {
        return;
    };
    let thumbnail_size = request.thumbnail_size.clamp(16, 256);
    let key = ThumbnailKey {
        path: path.clone(),
        modified: request.modified,
        file_size: request.file_size,
        thumbnail_size,
    };
    // Memory hits apply instantly with no I/O: revisits never wait on the
    // settle queue. Disk validation moves to fire time, where only rows in
    // the settled viewport spend the lookup; offscreen rows never touch the
    // disk merely because GTK retained them.
    match THUMBNAIL_CACHE.with(|cache| cache.borrow_mut().get(&key)) {
        Some(CacheHit::Ready(bytes)) => {
            apply_thumbnail(request.image, &bytes, thumbnail_size);
            return;
        }
        Some(CacheHit::Failed) => return,
        None => {}
    }
    let weak_image = glib::WeakRef::new();
    weak_image.set(Some(request.image));
    ACTIVE_REQUESTS.with(|requests| {
        requests.borrow_mut().insert(
            image_id,
            ActiveRequest {
                id: request_id,
                image: weak_image.clone(),
                deferred: None,
            },
        );
    });
    let target = PendingTarget {
        image_id,
        request: request_id,
        image: weak_image,
    };
    // Factory binds run while GTK is mutating the list's widget tree. Wait
    // until that mutation finishes before walking the image's ancestors and
    // hooking its viewport; doing either from the bind callback can corrupt
    // GTK's in-progress layout.
    glib::idle_add_local_once(move || {
        if target_live_image(&target).is_some() {
            park_thumbnail(key, kind, target, request.wait_for_metadata);
        }
    });
}

/// Viewport ancestor of a bound row image, if the row lives inside a
/// scrolled window. Resolved at park time, when the row is parented, so bind
/// call sites pass nothing new: every column, grid, explorer, and search row
/// finds its own viewport automatically. No scrolled ancestor (popups,
/// tests) lands in the fallback group with the historic behavior.
fn viewport_of(image: &gtk::Image) -> Option<gtk::ScrolledWindow> {
    let mut ancestor = image.parent();
    while let Some(widget) = ancestor {
        ancestor = widget.parent();
        if let Ok(viewport) = widget.downcast::<gtk::ScrolledWindow>() {
            return Some(viewport);
        }
    }
    None
}

/// Eligibility of an item rect expressed in viewport coordinates against the
/// viewport size. Visible rows plus `VIEWPORT_OVERSCAN` prefetch on every
/// side start work; anything further out waits for a later settle. Pure over
/// floats so the geometry is unit-testable without a display.
fn rect_eligible(
    x: f32,
    y: f32,
    width: f32,
    height: f32,
    viewport_width: f32,
    viewport_height: f32,
) -> bool {
    let overscan = VIEWPORT_OVERSCAN;
    width > 0.0
        && height > 0.0
        && viewport_width > 0.0
        && viewport_height > 0.0
        && x < viewport_width * (1.0 + overscan)
        && x + width > -viewport_width * overscan
        && y < viewport_height * (1.0 + overscan)
        && y + height > -viewport_height * overscan
}

/// The live item rect of a bound row image in viewport coordinates, or
/// `None` when the row is unmapped or detached. A miss degrades to
/// ineligible, never to a wrong row.
fn image_viewport_rect(
    image: &gtk::Image,
    viewport: &gtk::ScrolledWindow,
) -> Option<(f32, f32, f32, f32)> {
    let bounds = image.compute_bounds(viewport)?;
    Some((bounds.x(), bounds.y(), bounds.width(), bounds.height()))
}

fn list_item_owner(widget: &gtk::Widget) -> Option<gtk::Widget> {
    let mut child = widget.clone();
    while let Some(parent) = child.parent() {
        if parent.is::<gtk::ListView>() || parent.is::<gtk::GridView>() {
            return Some(child);
        }
        child = parent;
    }
    None
}

/// GTK keeps the last allocation of rows visited during a fling, so bounds
/// alone can make every intermediate page look current. Hit-testing the
/// visible part identifies the row actually displayed at that position.
fn image_is_currently_picked(
    image: &gtk::Image,
    viewport: &gtk::ScrolledWindow,
    x: f32,
    y: f32,
    width: f32,
    height: f32,
) -> bool {
    let left = x.max(0.0);
    let right = (x + width).min(viewport.width() as f32);
    let top = y.max(0.0);
    let bottom = (y + height).min(viewport.height() as f32);
    if left >= right || top >= bottom {
        return true;
    }
    let Some(picked) = viewport.pick(
        f64::from((left + right) / 2.0),
        f64::from((top + bottom) / 2.0),
        gtk::PickFlags::DEFAULT,
    ) else {
        return false;
    };
    match (
        list_item_owner(image.upcast_ref()),
        list_item_owner(&picked),
    ) {
        (Some(image), Some(picked)) => image == picked,
        _ => false,
    }
}

fn image_eligible(image: &gtk::Image, viewport: &gtk::ScrolledWindow) -> bool {
    let Some((x, y, width, height)) = image_viewport_rect(image, viewport) else {
        return false;
    };
    rect_eligible(
        x,
        y,
        width,
        height,
        viewport.width() as f32,
        viewport.height() as f32,
    ) && image_is_currently_picked(image, viewport, x, y, width, height)
}

/// Settle group address for a viewport: its widget address, or zero for the
/// fallback group. The map holds only weak viewport refs, so dead windows
/// drop out at the next fire instead of leaking.
fn group_address(viewport: Option<&gtk::ScrolledWindow>) -> usize {
    viewport.map_or(0, |viewport| viewport.as_ptr() as usize)
}

fn park_thumbnail(
    key: ThumbnailKey,
    kind: ThumbnailKind,
    target: PendingTarget,
    wait_for_metadata: bool,
) {
    crate::metrics::mark_thumbnail_requested(&key.path.display().to_string());
    // Parked until scrolling settles: a fling binds hundreds of rows, and
    // only the rows still visible at fire time earn a decode.
    let viewport = target.image.upgrade().and_then(|image| viewport_of(&image));
    let group = group_address(viewport.as_ref());
    let viewport_ref = glib::WeakRef::new();
    if let Some(viewport) = &viewport {
        viewport_ref.set(Some(viewport));
    }
    SETTLE_VIEWS.with(|views| {
        let mut views = views.borrow_mut();
        let settle = views.entry(group).or_insert_with(|| ViewSettle {
            viewport: viewport_ref.clone(),
            pending: Vec::new(),
            timer: None,
            first_park: None,
            hooked: false,
        });
        // A dead viewport's address may be recycled by a new window: never
        // join its stale hooks and pendings, reset the group instead. The
        // orphaned timer fires into the fresh group at worst, which only
        // settles early.
        if group != 0 && settle.viewport.upgrade().is_none() {
            if let Some(timer) = settle.timer.take() {
                timer.remove();
            }
            *settle = ViewSettle {
                viewport: viewport_ref.clone(),
                pending: Vec::new(),
                timer: None,
                first_park: None,
                hooked: false,
            };
        }
        settle.pending.push(SettledPark {
            key,
            kind,
            target,
            wait_for_metadata,
        });
        if settle.first_park.is_none() {
            settle.first_park = Some(Instant::now());
        }
    });
    if let Some(viewport) = viewport {
        hook_viewport(group, &viewport);
    }
    request_group_fire(group);
}

/// Parks into the legacy fallback group. Tests land here with the historic
/// global behavior.
#[cfg(test)]
fn schedule_or_defer(key: ThumbnailKey, kind: ThumbnailKind, target: PendingTarget) {
    park_thumbnail(key, kind, target, false);
}
fn mark_deferred(key: ThumbnailKey, kind: ThumbnailKind, image_id: usize, request: u64) {
    ACTIVE_REQUESTS.with(|requests| {
        if let Some(active) = requests
            .borrow_mut()
            .get_mut(&image_id)
            .filter(|active| active.id == request)
        {
            active.deferred = Some(DeferredThumbnail { key, kind });
        }
    });
}

/// Arms a group's settle timer. Fires happen exclusively on the main loop
/// through timer callbacks: `request_group_fire` runs inside binds and
/// adjustment callbacks, and a synchronous fire there would nest thumbnail
/// application inside GTK layout (reentrant relayout, torn item bounds,
/// fatal `bounds.y` assertion). An overdue group (continuous binds past
/// `MAX_SETTLE_WAIT`) arms a zero-delay timer instead of extending the
/// wait, so the bound holds without ever firing nested.
fn request_group_fire(group: usize) {
    SETTLE_VIEWS.with(|views| {
        let mut views = views.borrow_mut();
        let Some(settle) = views.get_mut(&group) else {
            return;
        };
        // No pending rows, no timer: adjustment callbacks fire constantly
        // during layout, and re-arming into an empty queue would turn every
        // thumbnail application (which relayouts) into another fire, a
        // self-sustaining apply storm that tears item bounds apart.
        if settle.pending.is_empty() {
            return;
        }
        let overdue = settle
            .first_park
            .is_some_and(|first| first.elapsed() >= MAX_SETTLE_WAIT);
        if let Some(timer) = settle.timer.take() {
            timer.remove();
        }
        let delay = if overdue {
            Duration::ZERO
        } else {
            THUMBNAIL_SETTLE_DELAY
        };
        settle.timer = Some(glib::timeout_add_local_once(delay, move || {
            fire_view_group(group);
        }));
    });
}
/// Hooks a viewport's adjustments so scrolling or resizing without fresh
/// binds still reconciles the parked set. Connected once per viewport; the
/// closures hold only the group address, so no reference cycle keeps a dead
/// window alive.
fn hook_viewport(group: usize, viewport: &gtk::ScrolledWindow) {
    let hooked = SETTLE_VIEWS.with(|views| {
        views
            .borrow_mut()
            .get_mut(&group)
            .map(|settle| std::mem::replace(&mut settle.hooked, true))
            .unwrap_or(true)
    });
    if hooked {
        return;
    }
    for adjustment in [viewport.vadjustment(), viewport.hadjustment()] {
        adjustment.connect_value_changed(move |_| request_group_fire(group));
        adjustment.connect_changed(move |_| request_group_fire(group));
    }
}

/// Fires one viewport group: drops dead windows, intersects the parked rows
/// with the live viewport, serves memory and disk caches for visible rows,
/// parks metadata waiters, and queues renders for the visible remainder.
fn fire_view_group(group: usize) {
    let (viewport, drained) = SETTLE_VIEWS.with(|views| {
        let mut views = views.borrow_mut();
        let Some(settle) = views.get_mut(&group) else {
            return (None, Vec::new());
        };
        if let Some(timer) = settle.timer.take() {
            timer.remove();
        }
        settle.first_park = None;
        let viewport = settle.viewport.upgrade();
        if group != 0 && viewport.is_none() {
            views.remove(&group);
            return (None, Vec::new());
        }
        let drained = std::mem::take(&mut settle.pending);
        (viewport, drained)
    });
    fire_parks(drained, viewport.as_ref());
}

/// Fires the fallback group. Tests drive this directly.
#[cfg(test)]
fn fire_settled_thumbnails() {
    // fire_view_group takes the group's timer, so no disarm is needed here.
    fire_view_group(0);
}

fn target_live_image(target: &PendingTarget) -> Option<gtk::Image> {
    let live = ACTIVE_REQUESTS.with(|requests| {
        requests
            .borrow()
            .get(&target.image_id)
            .is_some_and(|active| active.id == target.request)
    });
    if !live {
        return None;
    }
    target.image.upgrade()
}

fn fire_parks(mut drained: Vec<SettledPark>, viewport: Option<&gtk::ScrolledWindow>) {
    if let Some(viewport) = viewport {
        // Factory bind order is not a display-order contract. Queue the
        // settled viewport from top to bottom (and left to right for grids)
        // so the first visible item cannot be overtaken by a later row.
        drained.sort_by(|left, right| {
            let position = |park: &SettledPark| {
                park.target
                    .image
                    .upgrade()
                    .and_then(|image| image_viewport_rect(&image, viewport))
                    .map_or((f32::MAX, f32::MAX), |(x, y, _, _)| (y, x))
            };
            let left = position(left);
            let right = position(right);
            left.0
                .total_cmp(&right.0)
                .then_with(|| left.1.total_cmp(&right.1))
        });
    }
    let mut offscreen = Vec::new();
    let mut eligible = 0;
    let mut started = false;
    for park in drained {
        let image_id = park.target.image_id;
        let request = park.target.request;
        // Rows that scrolled away (or were unbound) while settling earn
        // nothing: their request was superseded or cancelled. Rows outside
        // the settled viewport wait for a later settle instead of decoding
        // off-screen: scrolling back re-parks them on rebind.
        let live = ACTIVE_REQUESTS.with(|requests| {
            requests
                .borrow()
                .get(&image_id)
                .is_some_and(|active| active.id == request)
        });
        if !live {
            continue;
        }
        // The widget may be gone while its request is still current (tests
        // park imageless targets): viewport, memory, and disk steps need the
        // live image, but the render queue decision does not. Completion
        // still drops imageless results as stale.
        let image = park.target.image.upgrade();
        if let (Some(image), Some(viewport)) = (image.as_ref(), viewport)
            && !image_eligible(image, viewport)
        {
            // GTK pre-binds rows outside its allocated window. Keep those
            // requests parked: adjustment changes recheck the same live
            // widgets, so scrolling into them starts work without a rebind.
            offscreen.push(park);
            continue;
        }
        eligible += 1;
        if let Some(image) = image.as_ref()
            && let Some(hit) = THUMBNAIL_CACHE.with(|cache| cache.borrow_mut().get(&park.key))
        {
            match hit {
                CacheHit::Ready(bytes) => {
                    apply_thumbnail(image, &bytes, park.key.thumbnail_size);
                }
                CacheHit::Failed => {}
            }
            continue;
        }
        if park.wait_for_metadata && park.key.modified.is_none() {
            push_metadata_waiter(group_of_viewport(viewport), park);
            continue;
        }
        if schedule_thumbnail(park.key.clone(), park.kind, park.target) {
            started = true;
        } else {
            mark_deferred(park.key, park.kind, image_id, request);
        }
    }
    if started {
        start_thumbnail_jobs();
    }
    crate::metrics::mark_thumbnail_eligible(eligible);
    if let Some(viewport) = viewport
        && !offscreen.is_empty()
    {
        let group = group_address(Some(viewport));
        SETTLE_VIEWS.with(|views| {
            if let Some(settle) = views.borrow_mut().get_mut(&group) {
                settle.pending.extend(offscreen);
            }
        });
    }
}

/// Settle group for a fire-time viewport reference.
fn group_of_viewport(viewport: Option<&gtk::ScrolledWindow>) -> usize {
    group_address(viewport)
}

fn push_metadata_waiter(group: usize, park: SettledPark) {
    METADATA_WAITERS.with(|waiters| {
        let mut waiters = waiters.borrow_mut();
        let queue = waiters.entry(park.key.path.clone()).or_default();
        // One waiter per bound view of the file is plenty; extras re-park
        // on their next bind.
        if queue.len() < 8 {
            queue.push(MetadataWaiter {
                group,
                kind: park.kind,
                target: park.target,
                file_size: park.key.file_size,
                thumbnail_size: park.key.thumbnail_size,
            });
        }
    });
}

/// Promotes rows parked while their file metadata was unknown. Called from
/// every metadata-fill path with the filled values; unknown mtimes no-op so
/// call sites stay one-liners. Each promoted waiter re-parks into its
/// original viewport group with a validated key, so the file renders once
/// instead of once without an mtime and again after it arrives.
pub(super) fn note_metadata(path: &Path, modified: Option<i64>, file_size: Option<u64>) {
    let Some(mtime) = modified else {
        return;
    };
    // Metadata may arrive after a bind parks the row but before the settle
    // timer moves it into METADATA_WAITERS. Update both sides of that gate so
    // the ordering cannot strand a live thumbnail indefinitely.
    SETTLE_VIEWS.with(|views| {
        for settle in views.borrow_mut().values_mut() {
            for park in &mut settle.pending {
                if park.wait_for_metadata && park.key.path == path {
                    park.key.modified = Some(mtime);
                    park.key.file_size = file_size.or(park.key.file_size);
                    park.wait_for_metadata = false;
                }
            }
        }
    });
    ACTIVE_REQUESTS.with(|requests| {
        for deferred in requests
            .borrow_mut()
            .values_mut()
            .filter_map(|active| active.deferred.as_mut())
        {
            if deferred.key.path == path {
                deferred.key.modified = Some(mtime);
                deferred.key.file_size = file_size.or(deferred.key.file_size);
            }
        }
    });
    let Some(waiters) = METADATA_WAITERS.with(|waiters| waiters.borrow_mut().remove(path)) else {
        return;
    };
    for waiter in waiters {
        if target_live_image(&waiter.target).is_none() {
            continue;
        }
        let key = ThumbnailKey {
            path: path.to_path_buf(),
            modified: Some(mtime),
            file_size: file_size.or(waiter.file_size),
            thumbnail_size: waiter.thumbnail_size,
        };
        park_into_group(waiter.group, key, waiter.kind, waiter.target, false);
    }
}
/// Fill-arm entry point: promotes metadata waiters for one filled entry.
/// Non-native locations and unknown mtimes no-op, so every fill arm calls
/// this unconditionally per update before touching its own rows.
pub(super) fn note_metadata_entry(entry: &FileEntry) {
    let (Some(path), Some(mtime)) = (
        entry.location.native_path(),
        known_metadata(&entry.modified_unix_seconds),
    ) else {
        return;
    };
    note_metadata(path, Some(mtime), known_metadata(&entry.size));
}

/// Parks into a known group (waiter promotion). A vanished group falls back
/// to the shared queue instead of dropping the row.
fn park_into_group(
    group: usize,
    key: ThumbnailKey,
    kind: ThumbnailKind,
    target: PendingTarget,
    wait_for_metadata: bool,
) {
    let known = SETTLE_VIEWS.with(|views| views.borrow().contains_key(&group));
    if !known && group != 0 {
        park_thumbnail(key, kind, target, wait_for_metadata);
        return;
    }
    SETTLE_VIEWS.with(|views| {
        let mut views = views.borrow_mut();
        let Some(settle) = views.get_mut(&group) else {
            return;
        };
        settle.pending.push(SettledPark {
            key,
            kind,
            target,
            wait_for_metadata,
        });
        if settle.first_park.is_none() {
            settle.first_park = Some(Instant::now());
        }
    });
    request_group_fire(group);
}

fn schedule_thumbnail(key: ThumbnailKey, kind: ThumbnailKind, target: PendingTarget) -> bool {
    PENDING_THUMBNAILS.with(|pending| {
        let mut pending = pending.borrow_mut();
        if let Some(pending) = pending.get_mut(&key) {
            pending.targets.push(target);
            true
        } else {
            let queued = THUMBNAIL_QUEUE.with(|queue| queue.borrow_mut().enqueue(key.clone()));
            if queued {
                pending.insert(
                    key.clone(),
                    PendingThumbnail {
                        id: NEXT_REQUEST.fetch_add(1, Ordering::Relaxed),
                        kind,
                        cancellation: Cancellation::default(),
                        targets: vec![target],
                    },
                );
            }
            queued
        }
    })
}

fn start_thumbnail_jobs() {
    while let Some(key) = THUMBNAIL_QUEUE.with(|queue| queue.borrow_mut().begin_next()) {
        let job = PENDING_THUMBNAILS.with(|pending| {
            pending.borrow().get(&key).map(|pending| ThumbnailJob {
                id: pending.id,
                key,
                kind: pending.kind,
                cancellation: pending.cancellation.clone(),
            })
        });
        let Some(job) = job else {
            THUMBNAIL_QUEUE.with(|queue| queue.borrow_mut().finish());
            continue;
        };
        crate::metrics::mark_thumbnail_started(&job.key.path.display().to_string());
        glib::MainContext::default().spawn_local(run_thumbnail_job(job));
    }
}

async fn run_thumbnail_job(job: ThumbnailJob) {
    let job_id = job.id;
    let key = job.key.clone();
    let thumbnail_size = key.thumbnail_size;
    let result = gio::spawn_blocking(move || {
        if let Some(mtime) = job.key.modified
            && let Some(png) = super::thumbnail_cache::lookup(&job.key.path, mtime)
        {
            return Ok((png, false));
        }
        // Always render the canonical `large` size class: the shared cache
        // keys one entry per file, so a small view's render must never
        // poison the bucket. Presentation downscales via `apply_thumbnail`.
        // Persistence deliberately stays out of this worker: storing beside
        // rendering holds a render slot behind disk I/O. Validated bytes
        // apply first and persist through the bounded background queue.
        render_thumbnail(
            &job.key.path,
            job.kind,
            super::thumbnail_cache::CANONICAL_MAX_EDGE,
            &job.cancellation,
        )
        .map(|png| (png, true))
    })
    .await;
    let targets = take_pending_targets(&key, job_id);
    THUMBNAIL_QUEUE.with(|queue| queue.borrow_mut().finish());
    let uri = key.path.display().to_string();

    if let Some(targets) = targets {
        match result {
            Ok(Ok((png, rendered))) => {
                crate::metrics::mark_thumbnail_completed(&uri);
                let bytes = glib::Bytes::from_owned(png.clone());
                THUMBNAIL_CACHE.with(|cache| cache.borrow_mut().insert(key.clone(), bytes.clone()));
                finish_thumbnail_targets(targets, Some(&bytes), thumbnail_size, &uri);
                // Validated bytes apply first; persistence follows on no
                // render slot. Unverifiable keys (unknown mtime) skip the
                // shared cache: nothing validates them later.
                if rendered && let Some(mtime) = key.modified {
                    enqueue_persist(key.path.clone(), mtime, png);
                }
            }
            Ok(Err(_)) | Err(_) => {
                crate::metrics::mark_thumbnail_cancelled(&uri);
                THUMBNAIL_CACHE.with(|cache| cache.borrow_mut().insert_failure(key));
                finish_thumbnail_targets(targets, None, thumbnail_size, &uri);
            }
        }
    }
    start_thumbnail_jobs();
    retry_deferred_thumbnails();
    let counts = crate::metrics::thumbnail_counts();
    tracing::debug!(?counts, "thumbnail pipeline settled");
}

fn retry_deferred_thumbnails() {
    let mut promoted = false;
    loop {
        // ponytail: deferred work is bounded by live GTK image widgets; add an explicit cap if a
        // future non-virtualized producer can create an unbounded number of them.
        let deferred = ACTIVE_REQUESTS.with(|requests| {
            let mut requests = requests.borrow_mut();
            requests.retain(|_, active| active.image.upgrade().is_some());
            requests
                .iter()
                .filter_map(|(image_id, active)| {
                    active.deferred.as_ref().map(|deferred| {
                        (*image_id, active.id, active.image.clone(), deferred.clone())
                    })
                })
                .min_by_key(|(_, request, _, _)| *request)
        });
        let Some((image_id, request, image, deferred)) = deferred else {
            break;
        };
        if !retry_deferred_thumbnail(image_id, request, image, deferred) {
            break;
        }
        promoted = true;
    }
    if promoted {
        start_thumbnail_jobs();
    }
}

fn retry_deferred_thumbnail(
    image_id: usize,
    request: u64,
    image: glib::WeakRef<gtk::Image>,
    deferred: DeferredThumbnail,
) -> bool {
    if !schedule_thumbnail(
        deferred.key,
        deferred.kind,
        PendingTarget {
            image_id,
            request,
            image,
        },
    ) {
        return false;
    }
    ACTIVE_REQUESTS.with(|requests| {
        if let Some(active) = requests
            .borrow_mut()
            .get_mut(&image_id)
            .filter(|active| active.id == request)
        {
            active.deferred = None;
        }
    });
    true
}

fn take_pending_targets(key: &ThumbnailKey, job_id: u64) -> Option<Vec<PendingTarget>> {
    PENDING_THUMBNAILS.with(|pending| {
        let mut pending = pending.borrow_mut();
        if pending.get(key).is_some_and(|pending| pending.id == job_id) {
            pending.remove(key).map(|pending| pending.targets)
        } else {
            None
        }
    })
}

fn finish_thumbnail_targets(
    targets: Vec<PendingTarget>,
    bytes: Option<&glib::Bytes>,
    thumbnail_size: i32,
    uri: &str,
) {
    for target in targets {
        let is_current = ACTIVE_REQUESTS.with(|requests| {
            let mut requests = requests.borrow_mut();
            if requests
                .get(&target.image_id)
                .is_some_and(|active| active.id == target.request)
            {
                requests.remove(&target.image_id);
                true
            } else {
                false
            }
        });
        if !is_current {
            crate::metrics::mark_thumbnail_stale(uri);
            continue;
        }
        let Some(bytes) = bytes else {
            continue;
        };
        let Some(image) = target.image.upgrade() else {
            crate::metrics::mark_thumbnail_stale(uri);
            continue;
        };
        apply_thumbnail(&image, bytes, thumbnail_size);
        crate::metrics::mark_thumbnail_applied(uri);
    }
}

fn known_metadata<T: Copy>(value: &MetadataValue<T>) -> Option<T> {
    match value {
        MetadataValue::Known(value) => Some(*value),
        MetadataValue::Unknown | MetadataValue::Unavailable => None,
    }
}

fn apply_thumbnail(image: &gtk::Image, bytes: &glib::Bytes, thumbnail_size: i32) {
    if let Ok(texture) = gdk::Texture::from_bytes(bytes) {
        crate::assets::remove_primary_icon(image);
        image.set_pixel_size(thumbnail_size);
        image.set_size_request(thumbnail_size, thumbnail_size);
        image.set_paintable(Some(&texture));
        image.set_opacity(1.0);
    }
}

pub(super) fn show_fallback_icon(image: &gtk::Image, icon: &str, size: i32) {
    set_fallback_icon(image, icon, size);
}

pub(super) fn cancel_list_item_thumbnails(item: &glib::Object) {
    let Some(item) = item.downcast_ref::<gtk::ListItem>() else {
        return;
    };
    if let Some(child) = item.child() {
        cancel_thumbnails_in(&child);
    }
}

pub(super) fn cancel_thumbnails_in(widget: &gtk::Widget) {
    if let Some(image) = widget.downcast_ref::<gtk::Image>() {
        cancel_thumbnail(image.as_ptr() as usize);
    }
    let mut child = widget.first_child();
    while let Some(current) = child {
        child = current.next_sibling();
        cancel_thumbnails_in(&current);
    }
}

fn set_fallback_icon(image: &gtk::Image, icon: &str, size: i32) -> (usize, u64) {
    let request = NEXT_REQUEST.fetch_add(1, Ordering::Relaxed);
    let image_id = image.as_ptr() as usize;
    cancel_thumbnail(image_id);
    image.set_pixel_size(size);
    image.set_size_request(size, size);
    crate::assets::set_primary_icon(image, icon);
    (image_id, request)
}

fn cancel_thumbnail(image_id: usize) {
    ACTIVE_REQUESTS.with(|requests| {
        requests.borrow_mut().remove(&image_id);
    });
    // Departed rows cancel everywhere they can wait: every viewport group
    // and the metadata-waiter map, so no queued work survives its row.
    SETTLE_VIEWS.with(|views| {
        views.borrow_mut().retain(|_, settle| {
            settle
                .pending
                .retain(|park| park.target.image_id != image_id);
            // Hooked viewports keep their group (and connections) across
            // empty moments; unhooked groups vanish once drained.
            !settle.pending.is_empty() || settle.hooked
        });
    });
    let cancelled = PENDING_THUMBNAILS.with(|pending| {
        let mut pending = pending.borrow_mut();
        let mut cancelled = Vec::new();
        pending.retain(|key, thumbnail| {
            thumbnail
                .targets
                .retain(|target| target.image_id != image_id);
            if thumbnail.targets.is_empty() {
                thumbnail.cancellation.cancel();
                cancelled.push(key.clone());
                false
            } else {
                true
            }
        });
        cancelled
    });
    THUMBNAIL_QUEUE.with(|queue| {
        let mut queue = queue.borrow_mut();
        for key in cancelled {
            queue.cancel(&key);
        }
    });
    retry_deferred_thumbnails();
}

fn thumbnail_kind(path: &Path) -> Option<ThumbnailKind> {
    let extension = path.extension()?.to_str()?.to_ascii_lowercase();
    match extension.as_str() {
        "png" | "jpg" | "jpeg" | "webp" | "gif" | "bmp" | "tif" | "tiff" => {
            Some(ThumbnailKind::Image)
        }
        "3fr" | "arw" | "cr2" | "cr3" | "dcr" | "dng" | "erf" | "kdc" | "mef" | "mos" | "mrw"
        | "nef" | "nrw" | "orf" | "pef" | "raf" | "raw" | "rw2" | "rwl" | "sr2" | "srf" | "srw"
        | "x3f" => Some(ThumbnailKind::RawImage),
        "pdf" => Some(ThumbnailKind::Pdf),
        "mp4" | "mkv" | "webm" | "mov" | "avi" | "m4v" | "mpeg" | "mpg" | "ogv" => {
            Some(ThumbnailKind::Video)
        }
        _ => None,
    }
}

fn render_thumbnail(
    path: &Path,
    kind: ThumbnailKind,
    size: i32,
    cancellation: &Cancellation,
) -> Result<Vec<u8>, String> {
    let operation = match kind {
        ThumbnailKind::Image => ParseOperation::ThumbnailImage,
        ThumbnailKind::RawImage => ParseOperation::ThumbnailRaw,
        ThumbnailKind::Pdf => ParseOperation::ThumbnailPdf,
        ThumbnailKind::Video => ParseOperation::ThumbnailVideo,
    };
    crate::sandbox::parse(
        path,
        operation,
        size.clamp(16, 256),
        crate::sandbox::MediaPreviewBackend::Software,
        cancellation,
    )
    .map(|output| output.data)
}

#[cfg(test)]
mod tests;
