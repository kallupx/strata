// SPDX-License-Identifier: GPL-3.0-or-later

use super::{LocationValidationError, backend_unavailable_message, validate_uri_credentials};

#[test]
fn embedded_uri_credentials_are_rejected() {
    for uri in [
        "smb://user:secret@host/share",
        "smb://user%3Asecret@host/share",
        "smb://user:sec%72et@host/share",
        "smb://user:@host/share",
        "smb://user;password=secret@host/share",
        "smb://user%3Bpassword=secret@host/share",
        "smb://user%3Bpassword%3Dsecret@host/share",
        "smb://user;password=sec%72et@host/share",
        "smb://user;@host/share",
        "sftp://user:secret@host:2222/path",
        "ftp://user:secret@host/public",
    ] {
        assert_eq!(
            validate_uri_credentials(uri),
            Err(LocationValidationError::EmbeddedCredential),
            "{uri:?} should be rejected"
        );
    }
}

#[test]
fn credential_free_uris_are_accepted() {
    for uri in [
        "smb://host/share",
        "smb://user@host/share",
        "sftp://user@host:2222/path",
        "network:///",
    ] {
        assert_eq!(
            validate_uri_credentials(uri),
            Ok(()),
            "{uri:?} should be safe"
        );
    }
}

#[test]
fn malformed_uris_fail_without_echoing_input() {
    assert_eq!(
        validate_uri_credentials("smb://user%ZZ@host/share"),
        Err(LocationValidationError::InvalidUri)
    );
    assert_eq!(
        LocationValidationError::InvalidUri.to_string(),
        "Enter a valid URI."
    );
}

#[test]
fn backend_unavailable_message_names_the_known_smb_package() {
    let message = backend_unavailable_message("smb://host/share");
    assert!(message.contains("smb://"));
    assert!(message.contains("gvfs-smb"));
}

#[test]
fn backend_unavailable_message_falls_back_for_unknown_schemes() {
    let message = backend_unavailable_message("dav://host/path");
    assert!(message.contains("dav://"));
}
