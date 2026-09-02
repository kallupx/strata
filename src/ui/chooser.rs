// SPDX-License-Identifier: GPL-3.0-or-later

#[cfg(test)]
mod tests;

use std::{
    cell::{Cell, RefCell},
    collections::HashMap,
    ffi::OsString,
    path::{Path, PathBuf},
    rc::Rc,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

use ashpd::{
    PortalError, WindowIdentifierType,
    desktop::file_chooser::{Choice, FileFilter, SelectedFiles},
};
use gtk::{gio, glib, prelude::*};

use crate::{
    adapters::{LocalFileSource, LocalOperationProvider, LocalPreviewProvider},
    app::BrowserEvent,
    model::{FileEntry, Location},
    portal::{
        ChooserKind, ChooserRequest, check_destinations, local_uri, open_selection, safe_filename,
        writable_from_read_only,
    },
    services::{
        DirectoryChange, DirectoryEvent, DirectoryRequest, FileSource, LoadHandle,
        LocationValidationError,
    },
};

use super::{
    browser::{BrowserView, column_menu_option, entry_supports_quick_preview},
    controls::{form_check_button, form_entry, form_label},
    preview::PreviewDrawer,
    theme::ThemeManager,
    window::{
        SidebarView, build_appearance_menu, build_sidebar, home_directory,
        is_sidebar_focus_shortcut, vim_focus_direction,
    },
};

type Completion = Box<dyn FnOnce(ashpd::backend::Result<SelectedFiles>)>;

thread_local! {
    static CHOOSERS: RefCell<HashMap<String, glib::WeakRef<gtk::Window>>> = RefCell::new(HashMap::new());
}

struct ChooserFileSource {
    filter: Rc<RefCell<Option<gtk::FileFilter>>>,
}

impl ChooserFileSource {
    fn new() -> Rc<Self> {
        Rc::new(Self {
            filter: Rc::new(RefCell::new(None)),
        })
    }

    fn set_filter(&self, filter: Option<gtk::FileFilter>) {
        self.filter.replace(filter);
    }
}

impl FileSource for ChooserFileSource {
    fn validate_location(&self, location: &Location) -> Result<(), LocationValidationError> {
        if location.native_path().is_none() {
            return Err(LocationValidationError::UnsupportedScheme(
                "The system file chooser supports local files and folders only.".into(),
            ));
        }
        LocalFileSource.validate_location(location)
    }

    fn enumerate(&self, request: DirectoryRequest, emit: Rc<dyn Fn(DirectoryEvent)>) -> LoadHandle {
        let filter = self.filter.clone();
        LocalFileSource.enumerate(
            request,
            Rc::new(move |event| {
                let event = match event {
                    DirectoryEvent::Batch {
                        request_id,
                        mut entries,
                    } => {
                        if let Some(filter) = filter.borrow().as_ref() {
                            entries.retain(|entry| file_filter_matches(filter, entry));
                        }
                        DirectoryEvent::Batch {
                            request_id,
                            entries,
                        }
                    }
                    event => event,
                };
                emit(event);
            }),
        )
    }

    fn watch(
        &self,
        location: Location,
        include_hidden: bool,
        notify: Rc<dyn Fn(DirectoryChange)>,
    ) -> Option<LoadHandle> {
        let filter = self.filter.clone();
        LocalFileSource.watch(
            location,
            include_hidden,
            Rc::new(move |change| {
                notify(filter_directory_change(filter.borrow().as_ref(), change));
            }),
        )
    }
}

fn file_filter_matches(filter: &gtk::FileFilter, entry: &FileEntry) -> bool {
    if entry.is_directory() {
        return true;
    }
    let info = gio::FileInfo::new();
    info.set_name(Path::new(&entry.native_name));
    info.set_display_name(&entry.display_name);
    info.set_file_type(gio::FileType::Regular);
    let (content_type, _) =
        gio::content_type_guess(Some(Path::new(&entry.native_name)), None::<&[u8]>);
    info.set_content_type(&content_type);
    filter.match_(&info)
}

fn filter_directory_change(
    filter: Option<&gtk::FileFilter>,
    change: DirectoryChange,
) -> DirectoryChange {
    match change {
        DirectoryChange::Upsert(entry)
            if filter.is_some_and(|filter| !file_filter_matches(filter, &entry)) =>
        {
            DirectoryChange::Remove(entry.location)
        }
        DirectoryChange::Move { from, entry }
            if filter.is_some_and(|filter| !file_filter_matches(filter, &entry)) =>
        {
            DirectoryChange::Remove(from)
        }
        change => change,
    }
}

fn chooser_preview_target(entry: Option<FileEntry>) -> Option<FileEntry> {
    entry.filter(entry_supports_quick_preview)
}

#[derive(Clone)]
struct PortalFilter {
    portal: FileFilter,
    native: gtk::FileFilter,
}

enum ChoiceControl {
    Boolean {
        id: String,
        check: gtk::CheckButton,
    },
    Select {
        id: String,
        values: Vec<String>,
        dropdown: ChooserDropdown,
    },
}

type SelectionChanged = Box<dyn Fn(usize)>;

struct ChooserDropdown {
    button: gtk::MenuButton,
    selected: Rc<Cell<usize>>,
    changed: Rc<RefCell<Option<SelectionChanged>>>,
}

impl ChooserDropdown {
    fn new(labels: &[&str], selected: usize) -> Self {
        let selected = selected.min(labels.len().saturating_sub(1));
        let current = labels.get(selected).copied().unwrap_or_default();
        let content = gtk::Box::new(gtk::Orientation::Vertical, 2);
        content.add_css_class("column-menu");
        let popover = gtk::Popover::builder()
            .child(&content)
            .has_arrow(false)
            .position(gtk::PositionType::Bottom)
            .build();
        popover.add_css_class("column-popover");
        let button = gtk::MenuButton::builder()
            .label(current)
            .popover(&popover)
            .build();
        button.add_css_class("form-control");
        button.set_halign(gtk::Align::Start);

        let selected = Rc::new(Cell::new(selected));
        let changed = Rc::new(RefCell::new(None::<SelectionChanged>));
        let checks = Rc::new(RefCell::new(Vec::<gtk::Image>::new()));
        for (index, label) in labels.iter().enumerate() {
            let (option, check) = column_menu_option(label, index == selected.get());
            checks.borrow_mut().push(check);
            let selected = selected.clone();
            let changed = changed.clone();
            let checks = checks.clone();
            let button = button.clone();
            let popover = popover.clone();
            let label = (*label).to_owned();
            option.connect_clicked(move |_| {
                selected.set(index);
                button.set_label(&label);
                for (check_index, check) in checks.borrow().iter().enumerate() {
                    check.set_visible(check_index == index);
                }
                popover.popdown();
                if let Some(changed) = changed.borrow().as_ref() {
                    changed(index);
                }
            });
            content.append(&option);
        }

        Self {
            button,
            selected,
            changed,
        }
    }

    fn selected(&self) -> usize {
        self.selected.get()
    }

    fn connect_selected(&self, callback: impl Fn(usize) + 'static) {
        self.changed.replace(Some(Box::new(callback)));
    }
}

impl ChoiceControl {
    fn value(&self) -> (String, String) {
        match self {
            Self::Boolean { id, check } => (id.clone(), check.is_active().to_string()),
            Self::Select {
                id,
                values,
                dropdown,
            } => (
                id.clone(),
                values.get(dropdown.selected()).cloned().unwrap_or_default(),
            ),
        }
    }
}

struct ChooserState {
    request: ChooserRequest,
    window: gtk::Window,
    view: BrowserView,
    filename: Option<gtk::Entry>,
    filter_dropdown: Option<ChooserDropdown>,
    filters: Vec<PortalFilter>,
    choices: Vec<ChoiceControl>,
    read_only: Option<gtk::CheckButton>,
    error: gtk::Label,
    completion: RefCell<Option<Completion>>,
}

impl ChooserState {
    fn cancel(&self) {
        self.finish(Err(PortalError::Cancelled("file chooser dismissed".into())));
    }

    fn finish(&self, result: ashpd::backend::Result<SelectedFiles>) {
        let Some(completion) = self.completion.take() else {
            return;
        };
        CHOOSERS.with(|choosers| {
            let mut choosers = choosers.borrow_mut();
            if choosers
                .get(&self.request.token)
                .and_then(glib::WeakRef::upgrade)
                .as_ref()
                .is_some_and(|window| window == &self.window)
            {
                choosers.remove(&self.request.token);
            }
        });
        self.window.close();
        completion(result);
    }

    fn show_error(&self, message: &str) {
        self.error.set_label(message);
        self.error.set_visible(true);
    }

    fn active_folder(&self) -> Result<PathBuf, &'static str> {
        self.view
            .browser()
            .active_location()
            .and_then(|location| location.native_path().map(Path::to_path_buf))
            .ok_or("Choose an accessible local folder")
    }

    fn selected_filter(&self) -> Option<FileFilter> {
        self.filter_dropdown
            .as_ref()
            .and_then(|dropdown| self.filters.get(dropdown.selected()))
            .map(|filter| filter.portal.clone())
    }

    fn selected_choices(&self) -> Vec<(String, String)> {
        self.choices.iter().map(ChoiceControl::value).collect()
    }

    fn complete_paths(&self, paths: Vec<PathBuf>, writable: Option<bool>) {
        let mut result = SelectedFiles::default();
        for path in paths {
            let uri = match local_uri(&path) {
                Ok(uri) => uri,
                Err(error) => {
                    self.finish(Err(error));
                    return;
                }
            };
            result = result.uri(uri);
        }
        for (id, value) in self.selected_choices() {
            result = result.choice(&id, &value);
        }
        result = result
            .current_filter(self.selected_filter())
            .writable(writable);
        self.finish(Ok(result));
    }

    fn accept(self: &Rc<Self>) {
        self.error.set_visible(false);
        match &self.request.kind {
            ChooserKind::Open {
                directory,
                multiple,
            } => {
                let browser = self.view.browser();
                let Some(current) = browser.active_location() else {
                    self.show_error("Choose an accessible local folder");
                    return;
                };
                match open_selection(&browser.selected_entries(), &current, *directory, *multiple) {
                    Ok(paths) => self.complete_paths(
                        paths,
                        self.read_only
                            .as_ref()
                            .map(|read_only| writable_from_read_only(read_only.is_active())),
                    ),
                    Err(message) => self.show_error(message),
                }
            }
            ChooserKind::SaveFile { .. } => self.accept_save_file(),
            ChooserKind::SaveFiles { names } => {
                let folder = match self.active_folder() {
                    Ok(folder) => folder,
                    Err(message) => {
                        self.show_error(message);
                        return;
                    }
                };
                self.accept_destinations(&folder, names);
            }
        }
    }

    fn accept_save_file(self: &Rc<Self>) {
        let Some(filename) = self.filename.as_ref() else {
            return;
        };
        let name = filename.text().to_string();
        if let Err(message) = crate::services::validate_basename(&name) {
            filename.add_css_class("error");
            filename.set_tooltip_text(Some(message));
            filename.grab_focus();
            self.show_error(message);
            return;
        }
        filename.remove_css_class("error");
        filename.set_tooltip_text(None);
        let folder = match self.active_folder() {
            Ok(folder) => folder,
            Err(message) => {
                self.show_error(message);
                return;
            }
        };
        let name = match &self.request.kind {
            ChooserKind::SaveFile {
                current_name: Some(current),
            } if current.to_string_lossy() == name => current.clone(),
            _ => OsString::from(name),
        };
        self.accept_destinations(&folder, &[name]);
    }

    fn accept_destinations(self: &Rc<Self>, folder: &Path, names: &[OsString]) {
        let destinations = match check_destinations(folder, names) {
            Ok(destinations) => destinations,
            Err(message) => {
                self.show_error(&message);
                return;
            }
        };
        if !destinations.existing_files {
            self.complete_paths(destinations.paths, None);
            return;
        }

        let plural = destinations.paths.len() > 1;
        let dialog = gtk::AlertDialog::builder()
            .modal(true)
            .message(if plural {
                "Replace existing files?"
            } else {
                "Replace existing file?"
            })
            .detail(if plural {
                "One or more destination files already exist. Continuing may overwrite them."
            } else {
                "A destination file already exists. Continuing may overwrite it."
            })
            .buttons(["Cancel", "Replace"])
            .cancel_button(0)
            .default_button(1)
            .build();
        let weak = Rc::downgrade(self);
        let window = self.window.clone();
        glib::MainContext::default().spawn_local(async move {
            if dialog.choose_future(Some(&window)).await == Ok(1)
                && let Some(state) = weak.upgrade()
            {
                state.complete_paths(destinations.paths, None);
            }
        });
    }

    fn activate_file(self: &Rc<Self>, location: &Location) {
        match &self.request.kind {
            ChooserKind::Open {
                directory: false, ..
            } => self.accept(),
            ChooserKind::SaveFile { .. } => {
                let Some((folder, name)) = location
                    .native_path()
                    .and_then(|path| Some((path.parent()?, path.file_name()?)))
                    .filter(|(_, name)| safe_filename(name))
                    .map(|(folder, name)| (folder.to_owned(), name.to_owned()))
                else {
                    return;
                };
                if let Some(filename) = self.filename.as_ref() {
                    filename.set_text(&name.to_string_lossy());
                }
                self.accept_destinations(&folder, &[name]);
            }
            _ => {}
        }
    }
}

pub(crate) fn present_chooser(
    request: ChooserRequest,
    cancelled: Arc<AtomicBool>,
    completion: impl FnOnce(ashpd::backend::Result<SelectedFiles>) + 'static,
) {
    if cancelled.load(Ordering::SeqCst) {
        completion(Err(PortalError::Cancelled(
            "file chooser request was cancelled".into(),
        )));
        return;
    }

    let source = ChooserFileSource::new();
    let (filters, selected_filter) =
        portal_filters(&request.filters, request.current_filter.as_ref());
    source.set_filter(
        selected_filter
            .and_then(|index| filters.get(index))
            .map(|filter| filter.native.clone()),
    );
    let multiple = matches!(&request.kind, ChooserKind::Open { multiple: true, .. });
    let view = BrowserView::new_chooser(source.clone(), multiple);
    let theme = ThemeManager::shared();
    view.set_view_mode(theme.browser_mode());
    view.set_density(theme.browser_density());
    view.set_peek_enabled(false);
    view.set_single_click_previews(false);
    view.set_operation_provider(Rc::new(LocalOperationProvider));
    let browser = view.browser();
    let preview = PreviewDrawer::new(Rc::new(LocalPreviewProvider), false);

    let window = gtk::Window::builder()
        .title(&request.title)
        .default_width(1050)
        .default_height(720)
        .modal(request.modal)
        .build();
    let header = gtk::HeaderBar::new();
    header.set_show_title_buttons(true);
    let sidebar_toggle = gtk::ToggleButton::builder()
        .active(true)
        .tooltip_text("Toggle sidebar (Ctrl+B)")
        .build();
    sidebar_toggle.set_child(Some(&crate::assets::primary_icon(
        crate::assets::icons::PANEL_LEFT,
        20,
    )));
    sidebar_toggle.add_css_class("sidebar-toggle");
    let location = view.location_widget();
    location.set_hexpand(true);
    let appearance = build_appearance_menu(&view, &browser, theme);
    let header_content = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    header_content.set_hexpand(true);
    header_content.append(&sidebar_toggle);
    header_content.append(&location);
    header_content.append(&appearance);
    header.set_title_widget(Some(&header_content));

    let sidebar = build_sidebar(view.clone(), true);
    let content = gtk::Paned::new(gtk::Orientation::Horizontal);
    content.set_position(208);
    content.set_shrink_start_child(false);
    content.set_resize_start_child(false);
    content.set_start_child(Some(&sidebar.widget));
    content.set_end_child(Some(&view.widget()));
    content.set_vexpand(true);
    let toggled_sidebar = sidebar.widget.clone();
    sidebar_toggle.connect_toggled(move |toggle| {
        toggled_sidebar.set_visible(toggle.is_active());
    });

    let preview_split = gtk::Paned::new(gtk::Orientation::Horizontal);
    preview_split.add_css_class("preview-split");
    preview_split.set_wide_handle(false);
    preview_split.set_resize_start_child(true);
    preview_split.set_resize_end_child(false);
    preview_split.set_shrink_start_child(false);
    preview_split.set_shrink_end_child(true);
    preview_split.set_start_child(Some(&content));
    preview_split.set_end_child(Some(&preview.widget()));
    preview_split.set_position(i32::MAX);
    preview_split.set_vexpand(true);
    let measured_content = content.clone();
    let measured_view = view.clone();
    preview.attach_split(
        &preview_split,
        Rc::new(move || measured_content.position() + measured_view.preview_occupied_width()),
    );

    let details = gtk::Box::new(gtk::Orientation::Vertical, 8);
    details.add_css_class("chooser-details");
    let filename = match &request.kind {
        ChooserKind::SaveFile { current_name } => {
            let row = labeled_row("Name", None::<&gtk::Widget>);
            let entry = form_entry();
            entry.set_hexpand(true);
            entry.set_placeholder_text(Some("Enter a filename"));
            if let Some(name) = current_name {
                entry.set_text(&name.to_string_lossy());
                entry.select_region(0, -1);
            }
            row.append(&entry);
            details.append(&row);
            Some(entry)
        }
        ChooserKind::SaveFiles { names } => {
            let names = names
                .iter()
                .map(|name| name.to_string_lossy())
                .collect::<Vec<_>>()
                .join(", ");
            let label = gtk::Label::new(Some(&names));
            label.add_css_class("action-dialog-description");
            label.set_xalign(0.0);
            label.set_ellipsize(gtk::pango::EllipsizeMode::Middle);
            label.set_tooltip_text(Some(&names));
            let row = labeled_row("Files", Some(label.upcast_ref()));
            details.append(&row);
            None
        }
        ChooserKind::Open { .. } => None,
    };

    let filter_dropdown = if filters.is_empty() {
        None
    } else {
        let labels = filters
            .iter()
            .map(|filter| filter.portal.label())
            .collect::<Vec<_>>();
        let dropdown = ChooserDropdown::new(&labels, selected_filter.unwrap_or(0));
        let row = labeled_row("Filter", Some(dropdown.button.upcast_ref()));
        details.append(&row);
        let filters_for_change = filters.clone();
        let source_for_change = source.clone();
        let browser_for_change = browser.clone();
        dropdown.connect_selected(move |selected| {
            source_for_change.set_filter(
                filters_for_change
                    .get(selected)
                    .map(|filter| filter.native.clone()),
            );
            if let Some(last) = browser_for_change.active_depth() {
                for depth in 0..=last {
                    browser_for_change.retry_column(depth);
                }
            }
        });
        Some(dropdown)
    };

    let choices = build_choices(&request.choices, &details);
    let read_only = matches!(&request.kind, ChooserKind::Open { .. }).then(|| {
        let check = form_check_button("Open files read-only");
        details.append(&check);
        check
    });

    let error = gtk::Label::new(None);
    error.add_css_class("form-message");
    error.add_css_class("error");
    error.set_xalign(0.0);
    error.set_wrap(true);
    error.set_visible(false);
    details.append(&error);

    let new_folder = gtk::Button::with_label("New Folder");
    new_folder.add_css_class("action-dialog-cancel");
    let cancel = gtk::Button::with_label("Cancel");
    cancel.add_css_class("action-dialog-cancel");
    let accept = gtk::Button::with_mnemonic(&request.accept_label);
    accept.add_css_class("action-dialog-confirm");
    let actions = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    actions.add_css_class("chooser-actions");
    actions.append(&new_folder);
    let spacer = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    spacer.set_hexpand(true);
    actions.append(&spacer);
    actions.append(&cancel);
    actions.append(&accept);
    details.append(&actions);

    let root = gtk::Box::new(gtk::Orientation::Vertical, 0);
    root.append(&header);
    root.append(&preview_split);
    root.append(&details);
    window.set_child(Some(&root));
    window.set_default_widget(Some(&accept));

    let state = Rc::new(ChooserState {
        request,
        window: window.clone(),
        view: view.clone(),
        filename: filename.clone(),
        filter_dropdown,
        filters,
        choices,
        read_only,
        error,
        completion: RefCell::new(Some(Box::new(completion))),
    });

    let weak = Rc::downgrade(&state);
    accept.connect_clicked(move |_| {
        if let Some(state) = weak.upgrade() {
            state.accept();
        }
    });
    let weak = Rc::downgrade(&state);
    cancel.connect_clicked(move |_| {
        if let Some(state) = weak.upgrade() {
            state.cancel();
        }
    });
    let new_folder_view = view.clone();
    new_folder.connect_clicked(move |_| new_folder_view.create_new_folder());
    if let Some(filename) = filename {
        let weak = Rc::downgrade(&state);
        filename.connect_activate(move |_| {
            if let Some(state) = weak.upgrade() {
                state.accept();
            }
        });
    }

    let state_for_observer = state.clone();
    let preview_for_selection = preview.clone();
    let weak_browser = Rc::downgrade(&browser);
    browser.observe(move |event| match event {
        BrowserEvent::OpenRequested { location } => state_for_observer.activate_file(&location),
        BrowserEvent::PreviewRequested { entry } => preview_for_selection.show(entry),
        BrowserEvent::FocusChanged {
            depth,
            position: Some(position),
        } if preview_for_selection.is_open() => {
            if let Some(entry) = weak_browser
                .upgrade()
                .and_then(|browser| browser.entry_at(depth, position))
                .and_then(|entry| chooser_preview_target(Some(entry)))
            {
                preview_for_selection.show(entry);
            } else {
                preview_for_selection.close();
            }
        }
        BrowserEvent::FocusChanged { position: None, .. } if preview_for_selection.is_open() => {
            preview_for_selection.close();
        }
        _ => {}
    });

    let weak = Rc::downgrade(&state);
    window.connect_close_request(move |_| {
        if let Some(state) = weak.upgrade() {
            state.cancel();
        }
        glib::Propagation::Proceed
    });
    install_shortcuts(&window, &state, &sidebar, &sidebar_toggle, &preview);
    let browser_for_destroy = browser.clone();
    window.connect_destroy(move |_| {
        browser_for_destroy.clear_observer();
        sidebar.disconnect();
    });

    let weak_window = glib::WeakRef::new();
    weak_window.set(Some(&window));
    CHOOSERS.with(|choosers| {
        let previous = {
            choosers
                .borrow_mut()
                .insert(state.request.token.clone(), weak_window)
                .and_then(|window| window.upgrade())
        };
        if let Some(previous) = previous {
            previous.close();
        }
    });
    if cancelled.load(Ordering::SeqCst) {
        state.cancel();
        return;
    }

    gtk::prelude::WidgetExt::realize(&window);
    apply_external_parent(&window, state.request.parent.as_ref());
    view.navigate(&state.request.initial_directory);
    window.present();
}

pub(crate) fn cancel_chooser(token: &str) {
    CHOOSERS.with(|choosers| {
        let window = {
            choosers
                .borrow()
                .get(token)
                .and_then(glib::WeakRef::upgrade)
        };
        if let Some(window) = window {
            window.close();
        }
    });
}

fn portal_filters(
    filters: &[FileFilter],
    current: Option<&FileFilter>,
) -> (Vec<PortalFilter>, Option<usize>) {
    let (filters, selected) = normalize_portal_filters(filters, current);
    (
        filters
            .into_iter()
            .map(|portal| {
                let native = gtk::FileFilter::new();
                native.set_name(Some(portal.label()));
                for pattern in portal.pattern_filters() {
                    native.add_pattern(pattern);
                }
                for mime in portal.mimetype_filters() {
                    native.add_mime_type(mime);
                }
                PortalFilter { portal, native }
            })
            .collect(),
        selected,
    )
}

fn normalize_portal_filters(
    filters: &[FileFilter],
    current: Option<&FileFilter>,
) -> (Vec<FileFilter>, Option<usize>) {
    let mut filters = filters.to_vec();
    if let Some(current) = current
        && !filters.contains(current)
    {
        filters.push(current.clone());
    }
    let selected = current
        .and_then(|current| filters.iter().position(|filter| filter == current))
        .or_else(|| (!filters.is_empty()).then_some(0));
    (filters, selected)
}

fn build_choices(choices: &[Choice], parent: &gtk::Box) -> Vec<ChoiceControl> {
    choices
        .iter()
        .map(|choice| {
            let pairs = choice.pairs();
            if pairs.is_empty() {
                let check = form_check_button(choice.label());
                check.set_active(choice.initial_selection() == "true");
                parent.append(&check);
                ChoiceControl::Boolean {
                    id: choice.id().to_owned(),
                    check,
                }
            } else {
                let labels = pairs.iter().map(|(_, label)| *label).collect::<Vec<_>>();
                let values = pairs
                    .iter()
                    .map(|(value, _)| (*value).to_owned())
                    .collect::<Vec<_>>();
                let selected = values
                    .iter()
                    .position(|value| value == choice.initial_selection())
                    .unwrap_or(0);
                let dropdown = ChooserDropdown::new(&labels, selected);
                let row = labeled_row(choice.label(), Some(dropdown.button.upcast_ref()));
                parent.append(&row);
                ChoiceControl::Select {
                    id: choice.id().to_owned(),
                    values,
                    dropdown,
                }
            }
        })
        .collect()
}

fn labeled_row(label: &str, child: Option<&gtk::Widget>) -> gtk::Box {
    let row = gtk::Box::new(gtk::Orientation::Horizontal, 10);
    let label = form_label(label);
    label.set_width_chars(12);
    row.append(&label);
    if let Some(child) = child {
        row.append(child);
    }
    row
}

fn apply_external_parent(window: &gtk::Window, parent: Option<&WindowIdentifierType>) {
    let Some(WindowIdentifierType::Wayland(handle)) = parent else {
        return;
    };
    let Some(surface) = window.surface() else {
        return;
    };
    let Ok(toplevel) = surface.downcast::<gdk4_wayland::WaylandToplevel>() else {
        tracing::debug!("portal parent type does not match the current display backend");
        return;
    };
    if !toplevel.set_transient_for_exported(handle) {
        tracing::debug!("Wayland compositor rejected the portal parent handle");
    }
}

fn install_shortcuts(
    window: &gtk::Window,
    state: &Rc<ChooserState>,
    sidebar: &SidebarView,
    sidebar_toggle: &gtk::ToggleButton,
    preview: &PreviewDrawer,
) {
    let keys = gtk::EventControllerKey::new();
    keys.set_propagation_phase(gtk::PropagationPhase::Capture);
    let weak = Rc::downgrade(state);
    let sidebar_state = sidebar.state.clone();
    let sidebar_widget = sidebar.widget.clone();
    let sidebar_toggle = sidebar_toggle.clone();
    let preview = preview.clone();
    let dialog_parent = window.clone();
    let focus_before_sidebar = Rc::new(RefCell::new(None::<gtk::Widget>));
    keys.connect_key_pressed(move |_, key, _, modifiers| {
        let Some(state) = weak.upgrade() else {
            return glib::Propagation::Proceed;
        };
        let browser = state.view.browser();
        let control = modifiers.contains(gtk::gdk::ModifierType::CONTROL_MASK);
        let alt = modifiers.contains(gtk::gdk::ModifierType::ALT_MASK);
        let shift = modifiers.contains(gtk::gdk::ModifierType::SHIFT_MASK);
        let focused = gtk::prelude::RootExt::focus(&dialog_parent);
        let sidebar_has_focus = focused.as_ref().is_some_and(|focused| {
            focused == &sidebar_widget || focused.is_ancestor(&sidebar_widget)
        });
        if key == gtk::gdk::Key::Escape {
            if state.view.cancel_new_entry() {
                return glib::Propagation::Stop;
            }
            if state.view.dismiss_focused_filter() {
                return glib::Propagation::Stop;
            }
            if state.view.location_has_focus() {
                state.view.cancel_location_edit();
                return glib::Propagation::Stop;
            }
            if preview.is_open() {
                preview.close();
                return glib::Propagation::Stop;
            }
            state.cancel();
            return glib::Propagation::Stop;
        }
        if state.view.new_entry_is_active() {
            return glib::Propagation::Proceed;
        }
        if control
            && !shift
            && !alt
            && matches!(key, gtk::gdk::Key::f | gtk::gdk::Key::F)
            && state.view.show_filter()
        {
            return glib::Propagation::Stop;
        }
        if control && matches!(key, gtk::gdk::Key::l | gtk::gdk::Key::L) {
            state.view.begin_location_edit();
            return glib::Propagation::Stop;
        }
        if is_sidebar_focus_shortcut(key, modifiers) {
            if sidebar_has_focus {
                let restored = focus_before_sidebar
                    .borrow_mut()
                    .take()
                    .is_some_and(|widget| widget.grab_focus());
                if !restored {
                    browser.focus_active();
                }
            } else {
                focus_before_sidebar.replace(focused.clone());
                if !sidebar_toggle.is_active() {
                    sidebar_toggle.set_active(true);
                }
                let sidebar = sidebar_state.clone();
                glib::idle_add_local_once(move || {
                    sidebar.focus_active_place();
                });
            }
            return glib::Propagation::Stop;
        }
        if control && !shift && matches!(key, gtk::gdk::Key::b | gtk::gdk::Key::B) {
            sidebar_toggle.set_active(!sidebar_toggle.is_active());
            return glib::Propagation::Stop;
        }
        if state.view.location_has_focus() {
            return glib::Propagation::Proceed;
        }
        if control && shift && matches!(key, gtk::gdk::Key::n | gtk::gdk::Key::N) {
            state.view.create_new_folder();
            return glib::Propagation::Stop;
        }
        if control
            && !shift
            && key == gtk::gdk::Key::a
            && !state.view.filter_has_focus()
            && matches!(
                &state.request.kind,
                ChooserKind::Open { multiple: true, .. }
            )
        {
            state.view.select_all();
            return glib::Propagation::Stop;
        }
        if control && matches!(key, gtk::gdk::Key::h | gtk::gdk::Key::H) {
            browser.toggle_hidden();
            return glib::Propagation::Stop;
        }
        let column_popover = focused
            .as_ref()
            .and_then(|focused| focused.ancestor(gtk::Popover::static_type()))
            .and_downcast::<gtk::Popover>()
            .filter(|popover| popover.has_css_class("column-popover"));
        if let Some(popover) = column_popover
            && !control
            && !alt
            && let Some(direction) = vim_focus_direction(key)
        {
            popover.child_focus(direction);
            return glib::Propagation::Stop;
        }
        let mut header_left_boundary = false;
        if state.view.header_actions_have_focus() && !control && !alt {
            match key {
                gtk::gdk::Key::h | gtk::gdk::Key::Left => {
                    if state.view.move_header_focus(gtk::DirectionType::Left) {
                        return glib::Propagation::Stop;
                    }
                    header_left_boundary = true;
                }
                gtk::gdk::Key::l | gtk::gdk::Key::Right => {
                    state.view.move_header_focus(gtk::DirectionType::Right);
                    return glib::Propagation::Stop;
                }
                gtk::gdk::Key::j | gtk::gdk::Key::Down => {
                    state.view.focus_items_from_header();
                    return glib::Propagation::Stop;
                }
                _ => {}
            }
        }
        if sidebar_has_focus
            && !control
            && !alt
            && let Some(direction) = vim_focus_direction(key)
        {
            if direction == gtk::DirectionType::Right {
                focus_before_sidebar.borrow_mut().take();
                browser.focus_active();
            } else {
                sidebar_widget.child_focus(direction);
            }
            return glib::Propagation::Stop;
        }
        if key == gtk::gdk::Key::BackSpace
            && !control
            && !alt
            && state.view.dismiss_empty_focused_filter()
        {
            return glib::Propagation::Stop;
        }
        if !control && !alt && !state.view.item_view_has_focus() && !header_left_boundary {
            return glib::Propagation::Proceed;
        }
        if key == gtk::gdk::Key::space && !control && !alt {
            preview.toggle(chooser_preview_target(browser.focused_entry()));
            return glib::Propagation::Stop;
        }
        if key == gtk::gdk::Key::BackSpace && !control && !alt {
            state.view.navigate_up();
            return glib::Propagation::Stop;
        }
        if shift
            && matches!(
                &state.request.kind,
                ChooserKind::Open { multiple: true, .. }
            )
            && key == gtk::gdk::Key::Up
        {
            browser.extend_selection(-1);
            return glib::Propagation::Stop;
        }
        if shift
            && matches!(
                &state.request.kind,
                ChooserKind::Open { multiple: true, .. }
            )
            && key == gtk::gdk::Key::Down
        {
            browser.extend_selection(1);
            return glib::Propagation::Stop;
        }
        if !shift
            && matches!(key, gtk::gdk::Key::k | gtk::gdk::Key::Up)
            && state.view.focus_header_from_top_item()
        {
            return glib::Propagation::Stop;
        }

        match (key, alt) {
            (gtk::gdk::Key::Left, true) => browser.back(),
            (gtk::gdk::Key::Right, true) => browser.forward(),
            (gtk::gdk::Key::Up, true) => browser.parent(),
            (gtk::gdk::Key::Home, true) => {
                browser.navigate(Location::local(home_directory()));
            }
            (gtk::gdk::Key::j | gtk::gdk::Key::Down, false) => browser.move_selection(1),
            (gtk::gdk::Key::k | gtk::gdk::Key::Up, false) => browser.move_selection(-1),
            (gtk::gdk::Key::h | gtk::gdk::Key::Left, false)
                if !control
                    && state.view.first_column_has_focus()
                    && sidebar_toggle.is_active() =>
            {
                focus_before_sidebar.replace(focused.clone());
                sidebar_state.focus_active_place();
            }
            (gtk::gdk::Key::h | gtk::gdk::Key::Left, false) => state.view.navigate_left(),
            (
                gtk::gdk::Key::l
                | gtk::gdk::Key::Right
                | gtk::gdk::Key::Return
                | gtk::gdk::Key::KP_Enter,
                false,
            ) => state.view.activate_focused(),
            _ => return glib::Propagation::Proceed,
        }
        glib::Propagation::Stop
    });
    window.add_controller(keys);
}
