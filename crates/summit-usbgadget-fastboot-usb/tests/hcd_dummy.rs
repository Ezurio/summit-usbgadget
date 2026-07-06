use std::error::Error;
use std::io::{self, Read, Write};
use std::time::Duration;

use nusb::{
    transfer::{Bulk, Direction, In, Out},
    MaybeFuture,
};
use summit_usbgadget_swupdate::SwupdateConfig;
use summit_usbgadget_swupdate::sysinfo;
use summit_usbgadget_usb::config::{DeviceConfig, FunctionConfig, GadgetConfig, UsbConfigConfig};
use summit_usbgadget_fastboot_usb::FastbootUsbFnConfig;

#[path = "../../summit-usbgadget-usb/tests/hcd_dummy_support/mod.rs"]
mod hcd_dummy_support;

use hcd_dummy_support::{
    GadgetServer, TEST_MANUFACTURER, TEST_SERIAL, TestEnvironment, dummy_hcd_test_lock,
    wait_for_usb_device,
    wait_for_usb_device_removal,
};

const FBK_VENDOR_ID: u16 = 0x1d50;
const FBK_PRODUCT_ID: u16 = 0x6152;
const FBK_CLOSE_PRODUCT_ID: u16 = 0x6154;
const FBK_PRODUCT: &str = "summit-usbgadget FBK test";

#[test]
#[ignore = "requires root, configfs, and dummy_hcd"]
fn hcd_dummy_fbk_protocol() -> Result<(), Box<dyn Error>> {
    let _guard = dummy_hcd_test_lock();
    let env = TestEnvironment::new()?;

    run_fbk_sequence(&env)
}

#[test]
#[ignore = "requires root, configfs, and dummy_hcd"]
fn hcd_dummy_fbk_close_edge_cases() -> Result<(), Box<dyn Error>> {
    let _guard = dummy_hcd_test_lock();
    let env = TestEnvironment::new()?;

    run_fbk_close_edge_cases(&env)
}

fn run_fbk_sequence(env: &TestEnvironment) -> Result<(), Box<dyn Error>> {
    env.set_output("fastboot-usb-fastboot.bin")?;

    let config = GadgetConfig {
        name: Some("summit-usbgadget-fastboot-usb-dummy-hcd".to_string()),
        udc: None,
        os_descriptor: None,
        device: DeviceConfig {
            vendor: FBK_VENDOR_ID,
            product: FBK_PRODUCT_ID,
            class: None,
            sub_class: None,
            protocol: None,
            manufacturer: Some(TEST_MANUFACTURER.to_string()),
            product_name: Some(FBK_PRODUCT.to_string()),
            product_name_source: Some("custom".to_string()),
            serial: Some(TEST_SERIAL.to_string()),
            serial_source: Some("custom".to_string()),
        },
        config: vec![UsbConfigConfig {
            description: Some("FBK protocol test".to_string()),
            max_power: Some(100),
            self_powered: Some(false),
            remote_wakeup: Some(false),
            function: vec![FunctionConfig::plugin(FastbootUsbFnConfig {
                swupdate: SwupdateConfig {
                    download: Some("swupdate".to_string()),
                    software_set: None,
                    image_mode: Some("complete".to_string()),
                    dry_run: Some(true),
                    disable_store_swu: Some(true),
                    timeout_secs: Some(5),
                },
            })],
        }],
    };

    let server = GadgetServer::spawn(config)?;
    let _device_path = wait_for_usb_device(FBK_VENDOR_ID, FBK_PRODUCT_ID, Duration::from_secs(10))?;
    let (interface, ep_in_addr, ep_out_addr) = claim_bulk_interface(FBK_VENDOR_ID, FBK_PRODUCT_ID, 0xff, 0x42, 0x03)?;

    let mut writer = interface.endpoint::<Bulk, Out>(ep_out_addr)?.writer(4096);
    let mut reader = interface.endpoint::<Bulk, In>(ep_in_addr)?.reader(4096);

    writer.write_all(b"getvar:serialno")?;
    writer.flush()?;
    assert_eq!(read_exact(&mut reader, 4 + TEST_SERIAL.len())?, format!("OKAY{TEST_SERIAL}").into_bytes());

    let expected_json = sysinfo::SystemInfo::collect(Some(TEST_SERIAL.to_string())).to_json_bytes();
    writer.write_all(b"fetch:sysinfo.json")?;
    writer.flush()?;
    let header = read_exact(&mut reader, 12)?;
    assert!(header.starts_with(b"DATA"));
    let fetch_len = parse_hex_len(&header[4..])?;
    let payload = read_exact(&mut reader, fetch_len)?;
    let okay = read_exact(&mut reader, 4)?;
    assert_eq!(payload, expected_json);
    assert_eq!(okay, b"OKAY");

    writer.write_all(b"download:%00000008ABCDEFGH")?;
    writer.flush()?;
    assert_eq!(read_exact(&mut reader, 12)?, b"DATA00000008");
    assert_eq!(read_exact(&mut reader, 4)?, b"OKAY");

    writer.write_all(b"flash:update")?;
    writer.flush()?;
    assert_eq!(read_exact(&mut reader, b"INFOwaiting for SWUpdate".len())?, b"INFOwaiting for SWUpdate");
    assert_eq!(read_exact(&mut reader, 4)?, b"OKAY");

    assert_eq!(env.read_output()?, b"ABCDEFGH");

    env.set_output("fastboot-usb-nxp.bin")?;

    writer.write_all(b"WOpen:update")?;
    writer.flush()?;
    assert_eq!(read_exact(&mut reader, 4)?, b"OKAY");

    writer.write_all(b"download:00000008ABCDEFGH")?;
    writer.flush()?;
    assert_eq!(read_exact(&mut reader, 12)?, b"DATA00000008");
    assert_eq!(read_exact(&mut reader, 4)?, b"OKAY");

    writer.write_all(b"Close")?;
    writer.flush()?;
    assert_eq!(read_exact(&mut reader, 4)?, b"OKAY");

    drop(writer);
    drop(reader);
    drop(interface);
    server.shutdown()?;
    wait_for_usb_device_removal(FBK_VENDOR_ID, FBK_PRODUCT_ID, Duration::from_secs(10))?;
    assert_eq!(env.read_output()?, b"ABCDEFGH");

    Ok(())
}

fn run_fbk_close_edge_cases(env: &TestEnvironment) -> Result<(), Box<dyn Error>> {
    env.set_output("fastboot-usb-close-empty.bin")?;

    let config = GadgetConfig {
        name: Some("summit-usbgadget-fastboot-usb-close-dummy-hcd".to_string()),
        udc: None,
        os_descriptor: None,
        device: DeviceConfig {
            vendor: FBK_VENDOR_ID,
            product: FBK_CLOSE_PRODUCT_ID,
            class: None,
            sub_class: None,
            protocol: None,
            manufacturer: Some(TEST_MANUFACTURER.to_string()),
            product_name: Some(FBK_PRODUCT.to_string()),
            product_name_source: Some("custom".to_string()),
            serial: Some(TEST_SERIAL.to_string()),
            serial_source: Some("custom".to_string()),
        },
        config: vec![UsbConfigConfig {
            description: Some("FBK close edge-case test".to_string()),
            max_power: Some(100),
            self_powered: Some(false),
            remote_wakeup: Some(false),
            function: vec![FunctionConfig::plugin(FastbootUsbFnConfig {
                swupdate: SwupdateConfig {
                    download: Some("swupdate".to_string()),
                    software_set: None,
                    image_mode: Some("complete".to_string()),
                    dry_run: Some(true),
                    disable_store_swu: Some(true),
                    timeout_secs: Some(5),
                },
            })],
        }],
    };

    let server = GadgetServer::spawn(config)?;
    let _device_path = wait_for_usb_device(FBK_VENDOR_ID, FBK_CLOSE_PRODUCT_ID, Duration::from_secs(10))?;
    let (interface, ep_in_addr, ep_out_addr) = claim_bulk_interface(FBK_VENDOR_ID, FBK_CLOSE_PRODUCT_ID, 0xff, 0x42, 0x03)?;

    let mut writer = interface.endpoint::<Bulk, Out>(ep_out_addr)?.writer(4096);
    let mut reader = interface.endpoint::<Bulk, In>(ep_in_addr)?.reader(4096);

    writer.write_all(b"Close")?;
    writer.flush()?;
    assert_eq!(read_exact(&mut reader, b"FAILclose".len())?, b"FAILclose");

    writer.write_all(b"WOpen:update")?;
    writer.flush()?;
    assert_eq!(read_exact(&mut reader, 4)?, b"OKAY");

    writer.write_all(b"Close")?;
    writer.flush()?;
    assert_eq!(read_exact(&mut reader, 4)?, b"OKAY");

    drop(writer);
    drop(reader);
    drop(interface);
    server.shutdown()?;
    wait_for_usb_device_removal(FBK_VENDOR_ID, FBK_CLOSE_PRODUCT_ID, Duration::from_secs(10))?;
    assert_eq!(env.read_output()?, b"");

    Ok(())
}

fn claim_bulk_interface(
    vendor_id: u16,
    product_id: u16,
    class: u8,
    subclass: u8,
    protocol: u8,
) -> Result<(nusb::Interface, u8, u8), Box<dyn Error>> {
    let device_info = wait_for_host_device(vendor_id, product_id, Duration::from_secs(5))?;
    let device = device_info.open().wait()?;
    let configuration = device.active_configuration()?;

    let descriptor = configuration
        .interface_alt_settings()
        .find(|desc| desc.class() == class && desc.subclass() == subclass && desc.protocol() == protocol)
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "matching bulk interface not found"))?;

    let mut ep_in = None;
    let mut ep_out = None;
    for endpoint in descriptor.endpoints() {
        match endpoint.direction() {
            Direction::In => ep_in = Some(endpoint.address()),
            Direction::Out => ep_out = Some(endpoint.address()),
        }
    }

    let interface = device.claim_interface(descriptor.interface_number()).wait()?;
    Ok((
        interface,
        ep_in.ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "bulk IN endpoint missing"))?,
        ep_out.ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "bulk OUT endpoint missing"))?,
    ))
}

fn read_exact(reader: &mut impl Read, len: usize) -> io::Result<Vec<u8>> {
    let mut buf = vec![0u8; len];
    reader.read_exact(&mut buf)?;
    Ok(buf)
}

fn parse_hex_len(bytes: &[u8]) -> io::Result<usize> {
    let text = std::str::from_utf8(bytes).map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))?;
    usize::from_str_radix(text, 16).map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))
}

fn wait_for_host_device(
    vendor: u16,
    product: u16,
    timeout: Duration,
) -> Result<nusb::DeviceInfo, Box<dyn Error>> {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        if let Some(device_info) = nusb::list_devices().wait()?.find(|device| {
            device.vendor_id() == vendor && device.product_id() == product
        }) {
            return Ok(device_info);
        }

        if std::time::Instant::now() >= deadline {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                format!("host did not observe USB device {:04x}:{:04x} within {:?}", vendor, product, timeout),
            )
            .into());
        }

        std::thread::sleep(Duration::from_millis(100));
    }
}