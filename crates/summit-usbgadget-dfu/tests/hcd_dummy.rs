use std::error::Error;
use std::io;
use std::time::Duration;

use nusb::{
    transfer::{ControlIn, ControlOut, ControlType, Recipient},
    MaybeFuture,
};
use summit_usbgadget_dfu::{DfuFnConfig, State as DfuState, Status as DfuStatus, request as dfu_request};
use summit_usbgadget_swupdate::{SwupdateConfig, sysinfo};
use summit_usbgadget_usb::config::{DeviceConfig, FunctionConfig, GadgetConfig, UsbConfigConfig};

#[path = "../../summit-usbgadget-usb/tests/hcd_dummy_support/mod.rs"]
mod hcd_dummy_support;

use hcd_dummy_support::{
    GadgetServer, TEST_MANUFACTURER, TestEnvironment, dummy_hcd_test_lock,
    read_trimmed, wait_for_usb_device,
    wait_for_usb_device_removal,
};

const DFU_VENDOR_ID: u16 = 0x1d50;
const DFU_PRODUCT_ID: u16 = 0x6151;
const DFU_ABORT_PRODUCT_ID: u16 = 0x6153;
const DFU_PRODUCT: &str = "summit-usbgadget DFU test";
const TEST_TRANSFER_SIZE: u16 = 32;

#[test]
#[ignore = "requires root, configfs, and dummy_hcd"]
fn hcd_dummy_dfu_protocol() -> Result<(), Box<dyn Error>> {
    let _guard = dummy_hcd_test_lock();
    let env = TestEnvironment::new()?;

    run_dfu_sequence(&env)
}

#[test]
#[ignore = "requires root, configfs, and dummy_hcd"]
fn hcd_dummy_dfu_abort_recovers() -> Result<(), Box<dyn Error>> {
    let _guard = dummy_hcd_test_lock();
    let env = TestEnvironment::new()?;

    run_dfu_abort_sequence(&env)
}

fn run_dfu_sequence(env: &TestEnvironment) -> Result<(), Box<dyn Error>> {
    env.set_output("dfu.bin")?;

    let config = GadgetConfig {
        name: Some("summit-usbgadget-dfu-dummy-hcd".to_string()),
        udc: None,
        os_descriptor: None,
        device: DeviceConfig {
            vendor: DFU_VENDOR_ID,
            product: DFU_PRODUCT_ID,
            class: None,
            sub_class: None,
            protocol: None,
            manufacturer: Some(TEST_MANUFACTURER.to_string()),
            product_name: Some(DFU_PRODUCT.to_string()),
            product_name_source: Some("custom".to_string()),
            serial: None,
            serial_source: Some("auto".to_string()),
        },
        config: vec![UsbConfigConfig {
            description: Some("DFU protocol test".to_string()),
            max_power: Some(100),
            self_powered: Some(false),
            remote_wakeup: Some(false),
            function: vec![FunctionConfig::plugin(DfuFnConfig {
                swupdate: SwupdateConfig {
                    download: Some("swupdate".to_string()),
                    software_set: None,
                    image_mode: Some("complete".to_string()),
                    dry_run: Some(true),
                    disable_store_swu: Some(true),
                    timeout_secs: Some(5),
                },
                upload: Some("sysinfo".to_string()),
                transfer_size: TEST_TRANSFER_SIZE,
                poll_timeout_ms: 10,
            })],
        }],
    };

    let server = GadgetServer::spawn(config)?;
    let device_path = wait_for_usb_device(DFU_VENDOR_ID, DFU_PRODUCT_ID, Duration::from_secs(10))?;
    assert_eq!(read_trimmed(&device_path.join("manufacturer"))?, TEST_MANUFACTURER);
    assert_eq!(read_trimmed(&device_path.join("product"))?, DFU_PRODUCT);
    let serial = read_trimmed(&device_path.join("serial"))?;
    assert!(!serial.is_empty());

    let (interface, if_num) = claim_interface(DFU_VENDOR_ID, DFU_PRODUCT_ID, 0xfe, 0x01, 0x02)?;

    let state = control_in(&interface, if_num, dfu_request::GETSTATE, 0, 1)?;
    assert_eq!(state, vec![DfuState::DfuIdle as u8]);

    let status = parse_dfu_status(&control_in(&interface, if_num, dfu_request::GETSTATUS, 0, 6)?)?;
    assert_eq!(status.status, DfuStatus::Ok as u8);
    assert_eq!(status.state, DfuState::DfuIdle as u8);

    let mut expected_upload = Vec::new();
    sysinfo::SystemInfo::collect(None).write_json(&mut expected_upload);
    let mut uploaded = Vec::new();
    let mut block = 0u16;
    loop {
        let chunk = control_in(&interface, if_num, dfu_request::UPLOAD, block, TEST_TRANSFER_SIZE)?;
        uploaded.extend_from_slice(&chunk);
        if chunk.len() < TEST_TRANSFER_SIZE as usize {
            break;
        }

        let upload_state = control_in(&interface, if_num, dfu_request::GETSTATE, 0, 1)?;
        assert_eq!(upload_state, vec![DfuState::UploadIdle as u8]);
        block = block.saturating_add(1);
    }
    assert_eq!(uploaded, expected_upload);

    control_out(&interface, if_num, dfu_request::DNLOAD, 0, b"abcde")?;
    let status = parse_dfu_status(&control_in(&interface, if_num, dfu_request::GETSTATUS, 0, 6)?)?;
    assert_eq!(status.status, DfuStatus::Ok as u8);
    assert!(matches!(status.state, x if x == DfuState::DnloadIdle as u8 || x == DfuState::DnBusy as u8));

    control_out(&interface, if_num, dfu_request::DNLOAD, 1, b"123")?;
    let status = parse_dfu_status(&control_in(&interface, if_num, dfu_request::GETSTATUS, 0, 6)?)?;
    assert_eq!(status.status, DfuStatus::Ok as u8);
    assert!(matches!(status.state, x if x == DfuState::DnloadIdle as u8 || x == DfuState::DnBusy as u8));

    control_out(&interface, if_num, dfu_request::DNLOAD, 2, &[])?;
    poll_until(Duration::from_secs(5), || {
        let status_bytes = control_in(&interface, if_num, dfu_request::GETSTATUS, 0, 6)
            .map_err(|err| io::Error::other(err.to_string()))?;
        let status = parse_dfu_status(&status_bytes)?;
        if status.status == DfuStatus::Ok as u8 && status.state == DfuState::DfuIdle as u8 {
            Ok(true)
        } else if status.state == DfuState::Error as u8 {
            Err(io::Error::other(format!("DFU entered error state after manifestation: {:?}", status)))
        } else {
            Ok(false)
        }
    })?;

    drop(interface);
    server.shutdown()?;
    wait_for_usb_device_removal(DFU_VENDOR_ID, DFU_PRODUCT_ID, Duration::from_secs(10))?;
    assert_eq!(env.read_output()?, b"abcde123");

    Ok(())
}

fn claim_interface(
    vendor_id: u16,
    product_id: u16,
    class: u8,
    subclass: u8,
    protocol: u8,
) -> Result<(nusb::Interface, u8), Box<dyn Error>> {
    let device_info = wait_for_host_device(vendor_id, product_id, Duration::from_secs(5))?;
    let device = device_info.open().wait()?;
    let configuration = device.active_configuration()?;

    let descriptor = configuration
        .interface_alt_settings()
        .find(|desc| desc.class() == class && desc.subclass() == subclass && desc.protocol() == protocol)
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "matching interface not found"))?;

    let if_num = descriptor.interface_number();
    let interface = device.claim_interface(if_num).wait()?;
    Ok((interface, if_num))
}

fn control_in(interface: &nusb::Interface, if_num: u8, request: u8, value: u16, length: u16) -> Result<Vec<u8>, Box<dyn Error>> {
    Ok(interface
        .control_in(
            ControlIn {
                control_type: ControlType::Class,
                recipient: Recipient::Interface,
                request,
                value,
                index: u16::from(if_num),
                length,
            },
            Duration::from_secs(2),
        )
        .wait()?
        .as_slice()
        .to_vec())
}

fn control_out(interface: &nusb::Interface, if_num: u8, request: u8, value: u16, data: &[u8]) -> Result<(), Box<dyn Error>> {
    interface
        .control_out(
            ControlOut {
                control_type: ControlType::Class,
                recipient: Recipient::Interface,
                request,
                value,
                index: u16::from(if_num),
                data,
            },
            Duration::from_secs(2),
        )
        .wait()?;
    Ok(())
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

fn run_dfu_abort_sequence(env: &TestEnvironment) -> Result<(), Box<dyn Error>> {
    env.set_output("dfu-abort.bin")?;

    let config = GadgetConfig {
        name: Some("summit-usbgadget-dfu-abort-dummy-hcd".to_string()),
        udc: None,
        os_descriptor: None,
        device: DeviceConfig {
            vendor: DFU_VENDOR_ID,
            product: DFU_ABORT_PRODUCT_ID,
            class: None,
            sub_class: None,
            protocol: None,
            manufacturer: Some(TEST_MANUFACTURER.to_string()),
            product_name: Some(DFU_PRODUCT.to_string()),
            product_name_source: Some("custom".to_string()),
            serial: None,
            serial_source: Some("auto".to_string()),
        },
        config: vec![UsbConfigConfig {
            description: Some("DFU abort test".to_string()),
            max_power: Some(100),
            self_powered: Some(false),
            remote_wakeup: Some(false),
            function: vec![FunctionConfig::plugin(DfuFnConfig {
                swupdate: SwupdateConfig {
                    download: Some("swupdate".to_string()),
                    software_set: None,
                    image_mode: Some("complete".to_string()),
                    dry_run: Some(true),
                    disable_store_swu: Some(true),
                    timeout_secs: Some(5),
                },
                upload: Some("sysinfo".to_string()),
                transfer_size: TEST_TRANSFER_SIZE,
                poll_timeout_ms: 10,
            })],
        }],
    };

    let server = GadgetServer::spawn(config)?;
    let _device_path = wait_for_usb_device(DFU_VENDOR_ID, DFU_ABORT_PRODUCT_ID, Duration::from_secs(10))?;
    let (interface, if_num) = claim_interface(DFU_VENDOR_ID, DFU_ABORT_PRODUCT_ID, 0xfe, 0x01, 0x02)?;

    control_out(&interface, if_num, dfu_request::DNLOAD, 0, b"abort-me")?;
    let status = parse_dfu_status(&control_in(&interface, if_num, dfu_request::GETSTATUS, 0, 6)?)?;
    assert_eq!(status.status, DfuStatus::Ok as u8);
    assert!(matches!(status.state, x if x == DfuState::DnloadIdle as u8 || x == DfuState::DnBusy as u8));

    control_out(&interface, if_num, dfu_request::ABORT, 0, &[])?;
    let state = control_in(&interface, if_num, dfu_request::GETSTATE, 0, 1)?;
    assert_eq!(state, vec![DfuState::DfuIdle as u8]);
    let status = parse_dfu_status(&control_in(&interface, if_num, dfu_request::GETSTATUS, 0, 6)?)?;
    assert_eq!(status.status, DfuStatus::Ok as u8);
    assert_eq!(status.state, DfuState::DfuIdle as u8);

    control_out(&interface, if_num, dfu_request::DNLOAD, 0, b"ok")?;
    let status = parse_dfu_status(&control_in(&interface, if_num, dfu_request::GETSTATUS, 0, 6)?)?;
    assert_eq!(status.status, DfuStatus::Ok as u8);
    assert!(matches!(status.state, x if x == DfuState::DnloadIdle as u8 || x == DfuState::DnBusy as u8));

    control_out(&interface, if_num, dfu_request::DNLOAD, 1, &[])?;
    poll_until(Duration::from_secs(5), || {
        let status_bytes = control_in(&interface, if_num, dfu_request::GETSTATUS, 0, 6)
            .map_err(|err| io::Error::other(err.to_string()))?;
        let status = parse_dfu_status(&status_bytes)?;
        if status.status == DfuStatus::Ok as u8 && status.state == DfuState::DfuIdle as u8 {
            Ok(true)
        } else if status.state == DfuState::Error as u8 {
            Err(io::Error::other(format!("DFU entered error state after abort recovery: {:?}", status)))
        } else {
            Ok(false)
        }
    })?;

    drop(interface);
    server.shutdown()?;
    wait_for_usb_device_removal(DFU_VENDOR_ID, DFU_ABORT_PRODUCT_ID, Duration::from_secs(10))?;
    assert_eq!(env.read_output()?, b"ok");

    Ok(())
}

fn poll_until<F>(timeout: Duration, mut check: F) -> io::Result<()>
where
    F: FnMut() -> io::Result<bool>,
{
    let deadline = std::time::Instant::now() + timeout;
    loop {
        if check()? {
            return Ok(());
        }
        if std::time::Instant::now() >= deadline {
            return Err(io::Error::new(io::ErrorKind::TimedOut, "timed out waiting for USB state"));
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[derive(Debug)]
struct ParsedDfuStatus {
    status: u8,
    state: u8,
}

fn parse_dfu_status(bytes: &[u8]) -> io::Result<ParsedDfuStatus> {
    if bytes.len() != 6 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("expected 6-byte DFU status, got {} bytes", bytes.len()),
        ));
    }

    Ok(ParsedDfuStatus {
        status: bytes[0],
        state: bytes[4],
    })
}