// SPDX-License-Identifier: GPL-3.0-or-later

use super::validate_basename;

#[test]
fn basenames_reject_empty_reserved_nested_absolute_and_nul_names() {
    for name in [
        "",
        ".",
        "..",
        "../escaped",
        "nested/child",
        "/tmp/absolute",
        "nul\0name",
    ] {
        assert!(
            validate_basename(name).is_err(),
            "{name:?} should be rejected"
        );
    }
}

#[test]
fn basenames_accept_single_native_and_unicode_components() {
    for name in ["report.txt", "folder name", ".config", "résumé"] {
        assert!(
            validate_basename(name).is_ok(),
            "{name:?} should be accepted"
        );
    }
}
