// SPDX-License-Identifier: GPL-3.0-or-later

//! Freedesktop shared thumbnail cache: reads and writes
//! `$XDG_CACHE_HOME/thumbnails/large` so thumbnails survive restarts and are
//! shared with every other spec-following application.
//!
//! Keys follow the thumbnail managing standard: the MD5 of the file's URI as
//! the file name. Every hit re-validates the PNG's own `Thumb::URI` and
//! `Thumb::MTime` text chunks against the requested file, so a stale or
//! foreign entry can never serve the wrong image; anything unreadable is a
//! miss. Only entries with a known modification time touch the disk at all,
//! since there is nothing to validate the cached copy against otherwise.
//!
//! Cache bytes are untrusted: entries are opened without following symlinks,
//! size- and dimension-bounded before allocation, structurally validated
//! before their tags are read, and never decoded outside the platform's
//! glycin-backed loader path. Raster decoding of source media happens only
//! inside the sandbox; this module merely persists and re-serves PNG bytes.
//! Stored entries are always normalized to the canonical `large` size class
//! (maximum edge of 256 px), so a small view can never poison the shared
//! bucket for larger views.

#[cfg(test)]
mod tests;

use std::{
    io::Read,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
};

/// Canonical `large` size class: every stored entry has a maximum edge of
/// exactly this many pixels, regardless of which view rendered it.
pub const CANONICAL_MAX_EDGE: i32 = 256;
/// Upper bound for one cache entry on disk. Canonical PNGs are tens of
/// kilobytes; anything past this is hostile or corrupt and is a miss without
/// being read.
const MAX_DISK_THUMBNAIL_BYTES: u64 = 2 * 1024 * 1024;
/// Upper bound for decoded cache dimensions. Canonical entries peak at 256
/// px; foreign spec-following `large` entries peak there too. Anything wider
/// is rejected before allocation.
const MAX_CACHED_DIMENSION: u32 = 512;
/// Upper bound for decoded cache pixels, bounding `width * height` before
/// any pixel buffer exists.
const MAX_CACHED_PIXELS: u64 = 512 * 512;
/// Upper bound for renders handed to `store`. Sandbox outputs are small; this
/// only guards the normalization decode against absurd inputs.
const MAX_RENDER_BYTES: usize = 32 * 1024 * 1024;

/// Reads a cached thumbnail for `path` validated at `mtime`, or `None` on any
/// miss, mismatch, or I/O failure. Callers fall back to rendering.
pub fn lookup(path: &Path, mtime: i64) -> Option<Vec<u8>> {
    let (uri, name) = cache_key(path)?;
    let bytes = read_bounded(&shared_cache_dir()?.join(name))?;
    let (width, height) = png_dimensions(&bytes)?;
    if width == 0 || height == 0 || width > MAX_CACHED_DIMENSION || height > MAX_CACHED_DIMENSION {
        return None;
    }
    if u64::from(width) * u64::from(height) > MAX_CACHED_PIXELS {
        return None;
    }
    // Tags validate only after the structure passed the bounded checks:
    // corrupt ancillary data is a miss, never a crash.
    let (stored_uri, stored_mtime) = read_thumb_tags(&bytes)?;
    if stored_uri != uri || stored_mtime != mtime.to_string() {
        return None;
    }
    Some(bytes)
}

/// Stores freshly rendered PNG bytes under the file's cache key, tagged with
/// its URI and mtime and normalized to the canonical size class. Best
/// effort: every failure is silently dropped and the in-memory result still
/// applies. Cancellation, vanished sources, and renderer failures never reach
/// here with bytes to persist, so nothing writes negative-cache files.
pub fn store(path: &Path, mtime: i64, png: &[u8]) {
    if png.is_empty() || png.len() > MAX_RENDER_BYTES {
        return;
    }
    let Some((uri, name)) = cache_key(path) else {
        return;
    };
    let Some(dir) = shared_cache_dir() else {
        return;
    };
    if ensure_cache_dir(&dir).is_err() {
        return;
    }
    let Ok(tagged) = normalize_to_canonical(png, &uri, mtime) else {
        return;
    };
    let _ignored = crate::storage::atomic_write(&dir.join(name), &tagged);
}

/// `(uri, "<md5>.png")` for `path`, or `None` when the path has no URI form.
/// The digest is only a file name, where MD5's collision weakness is
/// irrelevant; it comes from GLib's checksum support rather than a bespoke
/// implementation so keys match every other spec-following application.
fn cache_key(path: &Path) -> Option<(String, String)> {
    let uri = gtk_glib_filename_to_uri(path)?;
    Some((
        uri.clone(),
        format!("{}.png", glib_md5_hex(uri.as_bytes())?),
    ))
}

fn glib_md5_hex(data: &[u8]) -> Option<String> {
    let mut checksum = gtk::glib::Checksum::new(gtk::glib::ChecksumType::Md5)?;
    checksum.update(data);
    checksum.string()
}

fn gtk_glib_filename_to_uri(path: &Path) -> Option<String> {
    gtk::glib::filename_to_uri(path, None)
        .ok()
        .map(|uri| uri.to_string())
}

#[cfg(test)]
static CACHE_DIR_OVERRIDE: std::sync::Mutex<Option<PathBuf>> = std::sync::Mutex::new(None);

/// Test-only bucket override. Production always resolves through the
/// environment; mutating process-global environment in tests is both unsafe
/// (edition 2024) and racy, so tests serialize on a lock and swap this cell
/// instead.
#[cfg(test)]
fn set_cache_dir_override(dir: Option<PathBuf>) {
    match CACHE_DIR_OVERRIDE.lock() {
        Ok(mut slot) => *slot = dir,
        Err(poison) => {
            CACHE_DIR_OVERRIDE.clear_poison();
            *poison.into_inner() = dir;
        }
    }
}

#[cfg(test)]
fn cache_dir_override() -> Option<PathBuf> {
    match CACHE_DIR_OVERRIDE.lock() {
        Ok(slot) => slot.clone(),
        Err(poison) => {
            CACHE_DIR_OVERRIDE.clear_poison();
            poison.into_inner().clone()
        }
    }
}

fn shared_cache_dir() -> Option<PathBuf> {
    #[cfg(test)]
    if let Some(dir) = cache_dir_override() {
        return Some(dir);
    }
    let base = std::env::var_os("XDG_CACHE_HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME")
                .filter(|value| !value.is_empty())
                .map(|home| PathBuf::from(home).join(".cache"))
        })?;
    Some(base.join("thumbnails").join("large"))
}

/// Creates the cache directory with 0700 permissions even under a permissive
/// umask or XDG parent, and repairs the mode when the directory already
/// exists. Same-user applications are unaffected by the mode; other users
/// lose the ability to plant symlinks or FIFOs in the bucket.
fn ensure_cache_dir(dir: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
    Ok(())
}

/// Opens `path` without following symlinks, verifies it is a regular file,
/// and reads at most the entry bound. Symlinks, FIFOs, oversized entries,
/// and sparse files with huge claimed lengths are misses that never allocate
/// past the bound.
fn read_bounded(path: &Path) -> Option<Vec<u8>> {
    // `NONBLOCK`: opening a FIFO without it would wait for a writer
    // forever. The type check below rejects everything but regular files
    // before any byte is read.
    let fd = rustix::fs::open(
        path,
        rustix::fs::OFlags::RDONLY
            | rustix::fs::OFlags::NOFOLLOW
            | rustix::fs::OFlags::CLOEXEC
            | rustix::fs::OFlags::NONBLOCK,
        rustix::fs::Mode::empty(),
    )
    .ok()?;
    let stat = rustix::fs::fstat(&fd).ok()?;
    if rustix::fs::FileType::from_raw_mode(stat.st_mode) != rustix::fs::FileType::RegularFile {
        return None;
    }
    let file = std::fs::File::from(fd);
    let mut bytes = Vec::new();
    file.take(MAX_DISK_THUMBNAIL_BYTES + 1)
        .read_to_end(&mut bytes)
        .ok()?;
    if bytes.len() as u64 > MAX_DISK_THUMBNAIL_BYTES {
        return None;
    }
    Some(bytes)
}

/// Decodes rendered PNG bytes and re-saves them at the canonical size class,
/// tagged with the file's URI and mtime, so later lookups can validate the
/// entry. The output always has a maximum edge of 256 px: larger renders
/// scale down and smaller renders scale up, so a tiny view can never poison
/// the shared bucket with an upscaled-after-the-fact entry.
fn normalize_to_canonical(png: &[u8], uri: &str, mtime: i64) -> Result<Vec<u8>, String> {
    use gtk::gdk_pixbuf::prelude::PixbufLoaderExt;
    let loader = gtk::gdk_pixbuf::PixbufLoader::new();
    loader.write(png).map_err(|error| error.to_string())?;
    loader.close().map_err(|error| error.to_string())?;
    let pixbuf = loader
        .pixbuf()
        .ok_or_else(|| "thumbnail decoded to no image".to_owned())?;
    let (width, height) = (pixbuf.width(), pixbuf.height());
    if width <= 0 || height <= 0 {
        return Err("thumbnail decoded to empty dimensions".to_owned());
    }
    let long_edge = width.max(height);
    let pixbuf = if long_edge == CANONICAL_MAX_EDGE {
        pixbuf
    } else {
        let scale = f64::from(CANONICAL_MAX_EDGE) / f64::from(long_edge);
        let scaled_width = ((f64::from(width) * scale).round() as i32).max(1);
        let scaled_height = ((f64::from(height) * scale).round() as i32).max(1);
        pixbuf
            .scale_simple(
                scaled_width,
                scaled_height,
                gtk::gdk_pixbuf::InterpType::Bilinear,
            )
            .ok_or_else(|| "thumbnail scaling failed".to_owned())?
    };
    pixbuf
        .save_to_bufferv(
            "png",
            &[
                ("tEXt::Thumb::URI", uri),
                ("tEXt::Thumb::MTime", &mtime.to_string()),
            ],
        )
        .map_err(|error| error.to_string())
        .map(|bytes| bytes.to_vec())
}

/// Reads `(width, height)` out of a PNG's IHDR without decoding anything.
/// Returns `None` for anything that is not a PNG with a well-formed IHDR.
fn png_dimensions(png: &[u8]) -> Option<(u32, u32)> {
    const SIGNATURE: &[u8] = b"\x89PNG\r\n\x1a\n";
    if png.get(..SIGNATURE.len())? != SIGNATURE {
        return None;
    }
    let mut position = SIGNATURE.len();
    let length = u32::from_be_bytes(png.get(position..position + 4)?.try_into().ok()?) as usize;
    let kind = png.get(position + 4..position + 8)?;
    if kind != b"IHDR" || length < 13 {
        return None;
    }
    let data = png.get(position + 8..position + 8 + 13)?;
    let width = u32::from_be_bytes(data[0..4].try_into().ok()?);
    let height = u32::from_be_bytes(data[4..8].try_into().ok()?);
    position = position
        .checked_add(8)?
        .checked_add(length)?
        .checked_add(4)?;
    if position > png.len() {
        return None;
    }
    Some((width, height))
}

/// Reads the `Thumb::URI` / `Thumb::MTime` text tags out of PNG bytes.
/// Returns `None` for anything that is not a PNG or carries no tags, without
/// ever panicking on hostile input: the shared cache is world-writable data.
fn read_thumb_tags(png: &[u8]) -> Option<(String, String)> {
    const SIGNATURE: &[u8] = b"\x89PNG\r\n\x1a\n";
    if png.get(..SIGNATURE.len())? != SIGNATURE {
        return None;
    }
    let mut uri = None;
    let mut mtime = None;
    let mut position = SIGNATURE.len();
    while position.checked_add(8)? <= png.len() {
        let length = u32::from_be_bytes(png.get(position..position + 4)?.try_into().ok()?) as usize;
        let kind = png.get(position + 4..position + 8)?;
        let data_end = position.checked_add(8)?.checked_add(length)?;
        // Trailing CRC included: a truncated chunk ends the walk, never the process.
        if data_end.checked_add(4)? > png.len() {
            return None;
        }
        if kind == b"tEXt" {
            let data = &png[position + 8..data_end];
            if let Some(nul) = data.iter().position(|byte| *byte == 0) {
                let (keyword, value) = (&data[..nul], &data[nul + 1..]);
                if keyword == b"Thumb::URI" {
                    uri = std::str::from_utf8(value).ok().map(str::to_owned);
                } else if keyword == b"Thumb::MTime" {
                    mtime = std::str::from_utf8(value).ok().map(str::to_owned);
                }
            }
        }
        if kind == b"IEND" {
            break;
        }
        position = data_end + 4;
    }
    Some((uri?, mtime?))
}
