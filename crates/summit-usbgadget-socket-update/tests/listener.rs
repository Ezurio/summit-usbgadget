//
// SPDX-License-Identifier: LicenseRef-Ezurio-Clause
//

//! Integration tests for the socket update listener, exercising only its
//! public API.

use summit_usbgadget_socket_update::SocketSourceConfig;
use summit_usbgadget_swupdate::SwupdateConfig;

#[test]
fn swupdate_params_follow_socket_source_settings() {
    let config = SocketSourceConfig {
        address: "127.0.0.1:9000".to_string(),
        shutdown_timeout_secs: 7,
        inactivity_timeout_secs: 20,
        #[cfg(feature = "tls")]
        tls: None,
        swupdate: SwupdateConfig {
            download: None,
            software_set: Some("beta".to_string()),
            image_mode: Some("delta".to_string()),
            dry_run: Some(true),
            disable_store_swu: Some(false),
            timeout_secs: Some(55),
        },
    };

    let params = config.to_swupdate_params().unwrap();
    assert_eq!(params.software_set.as_deref(), Some("beta"));
    assert_eq!(params.image_mode.as_deref(), Some("delta"));
    assert!(params.dry_run);
    assert!(!params.disable_store_swu);
    assert_eq!(params.timeout.as_secs(), 55);
}
