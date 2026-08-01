//
// SPDX-License-Identifier: LicenseRef-Ezurio-Clause
//

use summit_usbgadget_config::sysinfo::nvmem::format_serial_mac;

#[test]
fn rejects_non_mac_cell_lengths() {
    assert_eq!(format_serial_mac(&[0x00, 0x1a, 0xb2]), None);
    assert_eq!(format_serial_mac(&[0x00; 7]), None);
}

#[test]
fn reverses_nvmem_bytes_for_serial_number() {
    assert_eq!(
        format_serial_mac(&[0x00, 0x1a, 0xb2, 0xc3, 0xd4, 0xef]),
        Some("efd4c3b21a00".to_string())
    );
}