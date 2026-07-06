//
// SPDX-License-Identifier: LicenseRef-Ezurio-Clause
//

//! Integration tests that unsupported DFU/FBK function sections are rejected by
//! their public `build` entry points.

use summit_usbgadget_dfu::DfuFnConfig;
use summit_usbgadget_fastboot_usb::FastbootUsbFnConfig;

#[test]
fn unsupported_dfu_section_is_skipped() {
    let config = DfuFnConfig {
        swupdate: summit_usbgadget_swupdate::SwupdateConfig {
            download: Some("swupdate".to_string()),
            ..summit_usbgadget_swupdate::SwupdateConfig::default()
        },
        upload: Some("unsupported".to_string()),
        transfer_size: None,
        poll_timeout_ms: None,
    };

    let result = summit_usbgadget_dfu::gadget::build(&config, "serial");
    assert!(result.is_err());
}

#[test]
fn unsupported_fbk_section_is_skipped() {
    let config = FastbootUsbFnConfig {
        swupdate: summit_usbgadget_swupdate::SwupdateConfig {
            download: Some("socket".to_string()),
            ..summit_usbgadget_swupdate::SwupdateConfig::default()
        },
    };

    let result = summit_usbgadget_fastboot_usb::build(&config, "serial");
    assert!(result.is_err());
}
