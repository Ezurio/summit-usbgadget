//
// SPDX-License-Identifier: LicenseRef-Ezurio-Clause
//

//! Integration tests for the SWUpdate configuration public API.

use summit_usbgadget_swupdate::{SwupdateConfig, SwupdateConfigError};

#[test]
fn socket_download_target_is_rejected() {
    let cfg = SwupdateConfig {
        download: Some("socket".to_string()),
        ..SwupdateConfig::default()
    };

    let err = cfg.to_params().expect_err("socket should not be accepted as a sink target");
    assert!(matches!(err, SwupdateConfigError::UnsupportedDownloadTarget(target) if target == "socket"));
}
