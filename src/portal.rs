// SPDX-License-Identifier: GPL-3.0-or-later

#[cfg(test)]
mod tests;

use std::{
    collections::HashMap,
    ffi::{OsStr, OsString},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
};

use ashpd::{
    MaybeAppID, PortalError, Uri, WindowIdentifierType,
    async_trait::async_trait,
    backend::{Builder, file_chooser::FileChooserImpl, request::RequestImpl},
    desktop::{
        HandleToken,
        file_chooser::{
            Choice, FileFilter, OpenFileOptions, SaveFileOptions, SaveFilesOptions, SelectedFiles,
        },
    },
};
use futures_channel::oneshot;
use gio::prelude::FileExt as _;

use crate::model::{FileEntry, Location};

const BACKEND_NAME: &str = "org.freedesktop.impl.portal.desktop.strata";
pub(crate) const FILE_CHOOSER_VERSION: u32 = 4;

#[derive(Debug)]
pub(crate) enum ChooserKind {
    Open { directory: bool, multiple: bool },
    SaveFile { current_name: Option<OsString> },
    SaveFiles { names: Vec<OsString> },
}

#[derive(Debug)]
pub(crate) struct ChooserRequest {
    pub token: String,
    pub title: String,
    pub accept_label: String,
    pub modal: bool,
    pub parent: Option<WindowIdentifierType>,
    pub initial_directory: PathBuf,
    pub kind: ChooserKind,
    pub filters: Vec<FileFilter>,
    pub current_filter: Option<FileFilter>,
    pub choices: Vec<Choice>,
}

#[derive(Default)]
struct RequestTracker {
    active: Mutex<HashMap<String, Arc<AtomicBool>>>,
}

impl RequestTracker {
    fn begin(self: &Arc<Self>, token: String) -> TrackedRequest {
        let cancelled = Arc::new(AtomicBool::new(false));
        self.active
            .lock()
            .expect("request tracker poisoned")
            .insert(token.clone(), cancelled.clone());
        TrackedRequest {
            tracker: self.clone(),
            token,
            cancelled,
        }
    }

    fn cancel(&self, token: &str) -> bool {
        let Some(cancelled) = self
            .active
            .lock()
            .expect("request tracker poisoned")
            .get(token)
            .cloned()
        else {
            return false;
        };
        !cancelled.swap(true, Ordering::SeqCst)
    }

    fn finish(&self, token: &str, cancelled: &Arc<AtomicBool>) {
        let mut active = self.active.lock().expect("request tracker poisoned");
        if active
            .get(token)
            .is_some_and(|current| Arc::ptr_eq(current, cancelled))
        {
            active.remove(token);
        }
    }
}

struct TrackedRequest {
    tracker: Arc<RequestTracker>,
    token: String,
    cancelled: Arc<AtomicBool>,
}

impl Drop for TrackedRequest {
    fn drop(&mut self) {
        self.tracker.finish(&self.token, &self.cancelled);
    }
}

#[derive(Clone, Default)]
struct FileChooserBackend {
    requests: Arc<RequestTracker>,
}

impl FileChooserBackend {
    async fn choose(&self, request: ChooserRequest) -> ashpd::backend::Result<SelectedFiles> {
        let token = request.token.clone();
        let tracked = self.requests.begin(token.clone());
        let cancelled = tracked.cancelled.clone();
        let (send, receive) = oneshot::channel();
        glib::MainContext::default().invoke(move || {
            crate::ui::present_chooser(request, cancelled, move |result| {
                let _result = send.send(result);
                drop(tracked);
            });
        });
        receive.await.unwrap_or_else(|_| {
            Err(PortalError::Cancelled(format!(
                "file chooser request {token} ended without a response"
            )))
        })
    }
}

#[async_trait]
impl RequestImpl for FileChooserBackend {
    async fn close(&self, token: HandleToken) {
        let token = token.to_string();
        self.requests.cancel(&token);
        glib::MainContext::default().invoke(move || crate::ui::cancel_chooser(&token));
    }
}

#[async_trait]
impl FileChooserImpl for FileChooserBackend {
    async fn open_file(
        &self,
        token: HandleToken,
        _app_id: Option<MaybeAppID>,
        parent: Option<WindowIdentifierType>,
        title: &str,
        options: OpenFileOptions,
    ) -> ashpd::backend::Result<SelectedFiles> {
        self.choose(open_request(token, parent, title, options))
            .await
    }

    async fn save_file(
        &self,
        token: HandleToken,
        _app_id: Option<MaybeAppID>,
        parent: Option<WindowIdentifierType>,
        title: &str,
        options: SaveFileOptions,
    ) -> ashpd::backend::Result<SelectedFiles> {
        self.choose(save_file_request(token, parent, title, options))
            .await
    }

    async fn save_files(
        &self,
        token: HandleToken,
        _app_id: Option<MaybeAppID>,
        parent: Option<WindowIdentifierType>,
        title: &str,
        options: SaveFilesOptions,
    ) -> ashpd::backend::Result<SelectedFiles> {
        self.choose(save_files_request(token, parent, title, options)?)
            .await
    }
}

pub(crate) fn run() -> glib::ExitCode {
    if let Err(error) = gtk::init() {
        eprintln!("Unable to initialize the Strata portal UI: {error}");
        return glib::ExitCode::FAILURE;
    }
    crate::metrics::initialize();
    if let Err(error) = tracing_subscriber::fmt::try_init() {
        eprintln!("Unable to initialize logging: {error}");
    }
    tracing::info!(
        version = FILE_CHOOSER_VERSION,
        "starting Strata FileChooser portal backend"
    );
    if let Err(error) = crate::assets::prepare() {
        eprintln!("Unable to prepare bundled assets: {error}");
    }
    crate::assets::register_icon_theme();
    crate::ui::prepare_portal_ui();

    let main_loop = glib::MainLoop::new(None, false);
    let service_loop = main_loop.clone();
    let service_failed = Arc::new(AtomicBool::new(false));
    let failed = service_failed.clone();
    std::thread::spawn(move || {
        let result = (|| {
            let pool = futures_executor::ThreadPool::new()
                .map_err(|error| PortalError::Failed(error.to_string()))?;
            let lost_loop = service_loop.clone();
            let service = Builder::new(BACKEND_NAME)?
                .with_spawn(pool)
                .with_name_lost(move || {
                    let lost_loop = lost_loop.clone();
                    glib::MainContext::default().invoke(move || lost_loop.quit());
                })
                .file_chooser(FileChooserBackend::default())
                .build();
            async_io::block_on(service)
        })();
        if let Err(error) = result {
            failed.store(true, Ordering::SeqCst);
            eprintln!("Strata portal backend failed: {error}");
            glib::MainContext::default().invoke(move || service_loop.quit());
        }
    });
    main_loop.run();
    if service_failed.load(Ordering::SeqCst) {
        glib::ExitCode::FAILURE
    } else {
        glib::ExitCode::SUCCESS
    }
}

fn open_request(
    token: HandleToken,
    parent: Option<WindowIdentifierType>,
    title: &str,
    options: OpenFileOptions,
) -> ChooserRequest {
    ChooserRequest {
        token: token.to_string(),
        title: request_title(title, "Open Files"),
        accept_label: options.accept_label().unwrap_or("Open").to_owned(),
        modal: options.modal().unwrap_or(true),
        parent,
        initial_directory: accessible_folder(options.current_folder().map(AsRef::as_ref)),
        kind: ChooserKind::Open {
            directory: options.directory().unwrap_or(false),
            multiple: options.multiple().unwrap_or(false),
        },
        filters: options.filters().to_vec(),
        current_filter: options.current_filter().cloned(),
        choices: options.choices().to_vec(),
    }
}

fn save_file_request(
    token: HandleToken,
    parent: Option<WindowIdentifierType>,
    title: &str,
    options: SaveFileOptions,
) -> ChooserRequest {
    let (initial_directory, current_name) = save_file_suggestion(
        options.current_file().map(AsRef::as_ref),
        options.current_folder().map(AsRef::as_ref),
        options.current_name(),
    );
    ChooserRequest {
        token: token.to_string(),
        title: request_title(title, "Save File"),
        accept_label: options.accept_label().unwrap_or("Save").to_owned(),
        modal: options.modal().unwrap_or(true),
        parent,
        initial_directory,
        kind: ChooserKind::SaveFile { current_name },
        filters: options.filters().to_vec(),
        current_filter: options.current_filter().cloned(),
        choices: options.choices().to_vec(),
    }
}

fn save_files_request(
    token: HandleToken,
    parent: Option<WindowIdentifierType>,
    title: &str,
    options: SaveFilesOptions,
) -> ashpd::backend::Result<ChooserRequest> {
    let names = options
        .files()
        .iter()
        .map(|path| path.as_ref().as_os_str().to_owned())
        .collect::<Vec<_>>();
    validate_save_filenames(&names)?;
    Ok(ChooserRequest {
        token: token.to_string(),
        title: request_title(title, "Save Files"),
        accept_label: options.accept_label().unwrap_or("Save").to_owned(),
        modal: options.modal().unwrap_or(true),
        parent,
        initial_directory: accessible_folder(options.current_folder().map(AsRef::as_ref)),
        kind: ChooserKind::SaveFiles { names },
        filters: Vec::new(),
        current_filter: None,
        choices: options.choices().to_vec(),
    })
}

fn request_title(title: &str, fallback: &str) -> String {
    if title.trim().is_empty() {
        fallback.to_owned()
    } else {
        title.to_owned()
    }
}

fn accessible_folder(suggestion: Option<&Path>) -> PathBuf {
    suggestion
        .filter(|path| path.is_absolute() && path.is_dir() && std::fs::read_dir(path).is_ok())
        .map(Path::to_path_buf)
        .unwrap_or_else(crate::ui::home_directory)
}

fn save_file_suggestion(
    current_file: Option<&Path>,
    current_folder: Option<&Path>,
    current_name: Option<&str>,
) -> (PathBuf, Option<OsString>) {
    if let Some(file) = current_file {
        if file.is_file()
            && let (Some(parent), Some(name)) = (file.parent(), file.file_name())
            && safe_filename(name)
            && accessible_folder(Some(parent)) == parent
        {
            return (parent.to_path_buf(), Some(name.to_owned()));
        }
        return (crate::ui::home_directory(), None);
    }

    let name = current_name
        .filter(|name| crate::services::validate_basename(name).is_ok())
        .map(OsString::from);
    (accessible_folder(current_folder), name)
}

pub(crate) fn safe_filename(name: &OsStr) -> bool {
    use std::os::unix::ffi::OsStrExt as _;

    let bytes = name.as_bytes();
    !bytes.is_empty()
        && !bytes.contains(&b'/')
        && !bytes.contains(&0)
        && !matches!(bytes, b"." | b"..")
}

pub(crate) fn writable_from_read_only(read_only: bool) -> bool {
    !read_only
}

fn validate_save_filenames(names: &[OsString]) -> ashpd::backend::Result<()> {
    if names.is_empty() {
        return Err(PortalError::InvalidArgument(
            "SaveFiles requires at least one filename".into(),
        ));
    }
    if names.iter().any(|name| !safe_filename(name)) {
        return Err(PortalError::InvalidArgument(
            "SaveFiles filenames must be safe basenames".into(),
        ));
    }
    Ok(())
}

pub(crate) fn local_uri(path: &Path) -> ashpd::backend::Result<Uri> {
    if !path.is_absolute() {
        return Err(PortalError::InvalidArgument(
            "file chooser results must be absolute local paths".into(),
        ));
    }
    let uri = gio::File::for_path(path).uri();
    if !uri.starts_with("file://") {
        return Err(PortalError::Failed(
            "GIO did not encode a local file URI".into(),
        ));
    }
    Uri::parse(&uri).map_err(|error| PortalError::Failed(error.to_string()))
}

pub(crate) fn open_selection(
    entries: &[FileEntry],
    current: &Location,
    directory: bool,
    multiple: bool,
) -> Result<Vec<PathBuf>, &'static str> {
    if entries.is_empty() && directory {
        return current
            .native_path()
            .map(|path| vec![path.to_path_buf()])
            .ok_or("Choose a local folder");
    }
    if entries.is_empty() {
        return Err("Choose a file");
    }
    if !multiple && entries.len() != 1 {
        return Err("Choose one item");
    }
    if entries
        .iter()
        .any(|entry| entry.is_directory() != directory)
    {
        return Err(if directory {
            "Choose folders only"
        } else {
            "Choose files only"
        });
    }
    entries
        .iter()
        .map(|entry| {
            entry
                .location
                .native_path()
                .map(Path::to_path_buf)
                .ok_or("Choose local items only")
        })
        .collect()
}

#[derive(Debug, Eq, PartialEq)]
pub(crate) struct DestinationCheck {
    pub paths: Vec<PathBuf>,
    pub existing_files: bool,
}

pub(crate) fn check_destinations(
    folder: &Path,
    names: &[OsString],
) -> Result<DestinationCheck, String> {
    if !folder.is_absolute() || !folder.is_dir() {
        return Err("Choose an accessible local folder".into());
    }
    let mut paths = Vec::with_capacity(names.len());
    let mut existing_files = false;
    for name in names {
        if !safe_filename(name) {
            return Err("Enter safe filenames without path separators".into());
        }
        let path = folder.join(name);
        match std::fs::symlink_metadata(&path) {
            Ok(_) if path.is_dir() => {
                return Err(format!(
                    "A folder named “{}” already exists",
                    name.to_string_lossy()
                ));
            }
            Ok(_) => existing_files = true,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(format!("Unable to inspect the destination: {error}")),
        }
        paths.push(path);
    }
    Ok(DestinationCheck {
        paths,
        existing_files,
    })
}
