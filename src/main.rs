// SPDX-License-Identifier: GPL-3.0-or-later

mod adapters;
mod app;
mod assets;
mod build_info;
mod metrics;
mod model;
mod sandbox;
mod sandbox_helper;
mod services;
mod storage;
#[cfg(test)]
mod test_support;
mod ui;
mod util;

use std::{os::unix::process::CommandExt, process::Stdio, time::Duration};

use gtk::{gio, prelude::*};

const APPLICATION_ID: &str = "io.github.lgse.Strata";
const GVFS_PROBE_ARGUMENT: &str = "--gvfs-probe";
const GVFS_PROBE_TIMEOUT: Duration = Duration::from_secs(2);

fn main() -> gtk::glib::ExitCode {
    let arguments: Vec<_> = std::env::args().collect();
    if arguments
        .get(1)
        .is_some_and(|value| value == "--preview-helper")
    {
        if let Err(error) = sandbox_helper::run(&arguments[2..]) {
            eprintln!("Preview helper failed: {error}");
            return gtk::glib::ExitCode::FAILURE;
        }
        return gtk::glib::ExitCode::SUCCESS;
    }
    if arguments
        .get(1)
        .is_some_and(|value| value == GVFS_PROBE_ARGUMENT)
    {
        let _vfs = gio::Vfs::default();
        let _volumes = gio::VolumeMonitor::get();
        return gtk::glib::ExitCode::SUCCESS;
    }

    metrics::initialize();
    if let Err(error) = tracing_subscriber::fmt::try_init() {
        eprintln!("Unable to initialize logging: {error}");
    }

    // Timed from here so `window presented` covers the whole launch, the way
    // the field harness measures mapped from process start.
    let launched = std::time::Instant::now();
    restart_with_local_vfs_if_gvfs_is_unresponsive();
    tracing::debug!(
        elapsed_ms = launched.elapsed().as_millis() as u64,
        "startup gvfs probe finished"
    );

    let assets_started = std::time::Instant::now();
    if let Err(error) = assets::prepare() {
        eprintln!("Unable to prepare bundled assets: {error}");
    }
    tracing::debug!(
        elapsed_ms = assets_started.elapsed().as_millis() as u64,
        total_elapsed_ms = launched.elapsed().as_millis() as u64,
        "startup bundled assets prepared"
    );

    let application = gtk::Application::builder()
        .application_id(APPLICATION_ID)
        .flags(gio::ApplicationFlags::HANDLES_OPEN)
        .build();

    application.connect_activate(ui::present);
    application.connect_open(|application, files, _| {
        let location = files.first().and_then(gio::File::path);
        ui::present_location(application, location);
    });
    application.run()
}

fn restart_with_local_vfs_if_gvfs_is_unresponsive() {
    if std::env::var_os("GIO_USE_VFS").is_some() {
        return;
    }
    // A healthy probe costs a full subprocess spawn (~60 ms warm) on every
    // launch. Remember the probed daemon generation instead: a wedged GVfs
    // daemon persists for the boot, so the first launch still catches it,
    // while the rest skip straight to startup. A daemon restart changes its
    // kernel pid, which re-arms the probe; a daemon wedging in place under
    // the same pid is not detected (documented residual: the probe timeout
    // bounded the original detection, and no cheap identity survives that).
    if gvfs_probe_marker_is_fresh() {
        return;
    }
    let Ok(executable) = std::env::current_exe() else {
        return;
    };
    let responsive = sandbox_helper::run_command_with_timeout(
        std::process::Command::new(&executable)
            .arg(GVFS_PROBE_ARGUMENT)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null()),
        GVFS_PROBE_TIMEOUT,
    )
    .unwrap_or(true);
    if responsive {
        write_gvfs_probe_marker();
        return;
    }

    eprintln!("GVFS is unresponsive; using local filesystem support for this session.");
    let error = std::process::Command::new(executable)
        .args(std::env::args_os().skip(1))
        .env("GIO_USE_VFS", "local")
        .exec();
    eprintln!("Unable to restart Strata with local filesystem support: {error}");
}

fn gvfs_probe_marker_path() -> Option<std::path::PathBuf> {
    gvfs_probe_marker_path_in(std::env::var_os("XDG_RUNTIME_DIR"))
}

fn gvfs_probe_marker_path_in(runtime: Option<std::ffi::OsString>) -> Option<std::path::PathBuf> {
    let runtime = runtime?;
    if runtime.is_empty() {
        return None;
    }
    Some(std::path::Path::new(&runtime).join("strata-gvfs-probe-ok"))
}

/// Kernel pids currently owned by a `gvfsd` process, sorted. Read straight
/// from `/proc` so a healthy launch pays directory reads instead of a
/// subprocess spawn. Matches the `gvfsd*` family (daemon, fuse, trash):
/// any of them restarting re-arms the probe.
fn gvfs_daemon_pids(proc_root: &std::path::Path) -> Vec<u32> {
    let mut pids = Vec::new();
    let Ok(entries) = std::fs::read_dir(proc_root) else {
        return pids;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(pid): Option<u32> = name.to_str().and_then(|name| name.parse().ok()) else {
            continue;
        };
        let comm = entry.path().join("comm");
        if std::fs::read_to_string(comm)
            .is_ok_and(|contents| contents.trim_end().starts_with("gvfsd"))
        {
            pids.push(pid);
        }
    }
    pids.sort_unstable();
    pids
}

fn encode_daemon_pids(pids: &[u32]) -> String {
    pids.iter()
        .map(u32::to_string)
        .collect::<Vec<_>>()
        .join(",")
}

fn gvfs_probe_marker_is_fresh() -> bool {
    let Some(path) = gvfs_probe_marker_path() else {
        return false;
    };
    let stored = std::fs::read_to_string(&path).unwrap_or_default();
    stored == encode_daemon_pids(&gvfs_daemon_pids(std::path::Path::new("/proc")))
}

fn write_gvfs_probe_marker() {
    let Some(path) = gvfs_probe_marker_path() else {
        return;
    };
    if let Some(parent) = path.parent() {
        let _ignored = std::fs::create_dir_all(parent);
    }
    let contents = encode_daemon_pids(&gvfs_daemon_pids(std::path::Path::new("/proc")));
    let _ignored = std::fs::write(&path, contents);
}

#[cfg(test)]
mod tests;
