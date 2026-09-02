// SPDX-License-Identifier: GPL-3.0-or-later

use std::fs;

use super::{SetupContext, disable_config, enable_config, install_at, uninstall_at};

const FILE_CHOOSER: &str = "org.freedesktop.impl.portal.FileChooser";

#[test]
fn chooser_preference_uses_existing_backends_and_round_trips() {
    let original = "[preferred]\ndefault=hyprland;gtk;\norg.example.Other=gtk;\n";
    let enabled = enable_config(original).expect("enable Strata");

    assert!(enabled.contains(&format!("{FILE_CHOOSER}=strata;hyprland;gtk;")));
    assert_eq!(enable_config(&enabled).expect("enable again"), enabled);
    assert_eq!(disable_config(&enabled).expect("disable Strata"), original);
}

#[test]
fn chooser_preference_preserves_explicit_fallbacks() {
    let original = format!("[preferred]\ndefault=gnome;gtk;\n{FILE_CHOOSER}=kde;gtk;\n");
    let enabled = enable_config(&original).expect("enable Strata");

    assert!(enabled.contains(&format!("{FILE_CHOOSER}=strata;kde;gtk;")));
    assert_eq!(disable_config(&enabled).expect("disable Strata"), original);
}

#[test]
fn install_and_uninstall_restore_an_existing_user_configuration() {
    let fixture = tempfile::tempdir().expect("fixture directory");
    let context = context(fixture.path());
    let config = context.portal_directory().join("portals.conf");
    fs::create_dir_all(config.parent().expect("config parent")).expect("config directory");
    let original = "[preferred]\ndefault=gtk;\n";
    fs::write(&config, original).expect("portal config");

    let installed_path =
        install_at(&context, &fixture.path().join("bin/strata")).expect("install portal");

    assert_eq!(installed_path, config);
    assert!(
        fs::read_to_string(&config)
            .expect("installed config")
            .contains("strata;gtk;")
    );
    assert!(
        context
            .data_home
            .join("xdg-desktop-portal/portals/strata.portal")
            .is_file()
    );
    assert!(
        fs::read_to_string(
            context
                .data_home
                .join("dbus-1/services/org.freedesktop.impl.portal.desktop.strata.service")
        )
        .expect("D-Bus service")
        .contains(&format!(
            "Exec={}/bin/strata --portal",
            fixture.path().display()
        ))
    );

    install_at(&context, &fixture.path().join("bin/strata")).expect("install portal again");

    assert!(!uninstall_at(&context).expect("uninstall portal"));
    assert_eq!(
        fs::read_to_string(config).expect("restored config"),
        original
    );
}

#[test]
fn uninstall_keeps_later_configuration_edits() {
    let fixture = tempfile::tempdir().expect("fixture directory");
    let context = context(fixture.path());
    let config = context.portal_directory().join("portals.conf");
    fs::create_dir_all(config.parent().expect("config parent")).expect("config directory");
    fs::write(&config, "[preferred]\ndefault=gtk;\n").expect("portal config");
    install_at(&context, &fixture.path().join("strata")).expect("install portal");
    fs::write(
        &config,
        format!(
            "[preferred]\ndefault=gtk;\n{FILE_CHOOSER}=strata;gtk;\norg.example.Other=custom;\n"
        ),
    )
    .expect("edit portal config");

    assert!(uninstall_at(&context).expect("uninstall portal"));
    let remaining = fs::read_to_string(config).expect("preserved config");
    assert!(!remaining.contains("strata"));
    assert!(remaining.contains("org.example.Other=custom;"));
}

#[test]
fn generated_override_is_removed_on_uninstall() {
    let fixture = tempfile::tempdir().expect("fixture directory");
    let context = context(fixture.path());
    let system_config = context.search_roots[1].join("xdg-desktop-portal/hyprland-portals.conf");
    fs::create_dir_all(system_config.parent().expect("system config parent"))
        .expect("system config directory");
    fs::write(&system_config, "[preferred]\ndefault=hyprland;gtk;\n").expect("system config");

    let generated = install_at(&context, &fixture.path().join("strata")).expect("install portal");

    assert!(generated.ends_with("hyprland-portals.conf"));
    assert!(
        fs::read_to_string(&generated)
            .expect("generated config")
            .contains("strata;")
    );
    assert!(!uninstall_at(&context).expect("uninstall portal"));
    assert!(!generated.exists());
}

fn context(root: &std::path::Path) -> SetupContext {
    let data_home = root.join("data");
    let config_home = root.join("config");
    SetupContext {
        search_roots: vec![config_home.clone(), root.join("system")],
        data_home,
        config_home,
        config_names: vec![
            "hyprland-portals.conf".to_owned(),
            "portals.conf".to_owned(),
        ],
    }
}
