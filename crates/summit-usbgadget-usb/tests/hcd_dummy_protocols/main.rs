use std::error::Error;
use std::ffi::OsString;
use std::fs;
use std::io::{self, Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
    Mutex,
    MutexGuard,
    OnceLock,
};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use nusb::{
    transfer::{Bulk, ControlIn, ControlOut, ControlType, Direction, In, Out, Recipient},
    MaybeFuture,
};

use summit_usbgadget_dfu::{request as dfu_request, DfuFnConfig, State as DfuState, Status as DfuStatus};
use summit_usbgadget_fastboot_usb::FastbootUsbFnConfig;
use summit_usbgadget_swupdate::{sysinfo, SwupdateConfig};
use summit_usbgadget_usb::config::{DeviceConfig, FunctionConfig, GadgetConfig, UsbConfigConfig};
use summit_usbgadget_usb::gadget;

const DFU_VENDOR_ID: u16 = 0x1d50;
const DFU_PRODUCT_ID: u16 = 0x6151;
const DFU_ABORT_PRODUCT_ID: u16 = 0x6153;
const FBK_VENDOR_ID: u16 = 0x1d50;
const FBK_PRODUCT_ID: u16 = 0x6152;
const FBK_CLOSE_PRODUCT_ID: u16 = 0x6154;
const TEST_MANUFACTURER: &str = "Ezurio";
const DFU_PRODUCT: &str = "summit-usbgadget DFU test";
const FBK_PRODUCT: &str = "summit-usbgadget FBK test";
const TEST_SERIAL: &str = "dummy-hcd-protocols";
const TEST_TRANSFER_SIZE: u16 = 32;
const USB_TIMEOUT: Duration = Duration::from_secs(2);

#[test]
#[ignore = "requires root, configfs, and dummy_hcd"]
fn hcd_dummy_dfu_and_fbk_protocols() -> Result<(), Box<dyn Error>> {
    let _guard = dummy_hcd_test_lock();
    let env = TestEnvironment::new()?;

    run_dfu_sequence(&env)?;
    run_fbk_sequence(&env)?;

    Ok(())
}

#[test]
#[ignore = "requires root, configfs, and dummy_hcd"]
fn hcd_dummy_dfu_abort_recovers() -> Result<(), Box<dyn Error>> {
    let _guard = dummy_hcd_test_lock();
    let env = TestEnvironment::new()?;

    run_dfu_abort_sequence(&env)?;

    Ok(())
}

#[test]
#[ignore = "requires root, configfs, and dummy_hcd"]
fn hcd_dummy_fbk_close_edge_cases() -> Result<(), Box<dyn Error>> {
    let _guard = dummy_hcd_test_lock();
    let env = TestEnvironment::new()?;

    run_fbk_close_edge_cases(&env)?;

    Ok(())
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
            serial: Some(TEST_SERIAL.to_string()),
            serial_source: Some("custom".to_string()),
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
    assert_eq!(read_trimmed(&device_path.join("serial"))?, TEST_SERIAL);

    let (interface, if_num) = claim_interface(DFU_VENDOR_ID, DFU_PRODUCT_ID, 0xfe, 0x01, 0x02)?;

    let state = control_in(&interface, if_num, dfu_request::GETSTATE, 0, 1)?;
    assert_eq!(state, vec![DfuState::DfuIdle as u8]);

    let status = parse_dfu_status(&control_in(&interface, if_num, dfu_request::GETSTATUS, 0, 6)?)?;
    assert_eq!(status.status, DfuStatus::Ok as u8);
    assert_eq!(status.state, DfuState::DfuIdle as u8);

    let mut expected_upload = Vec::new();
    sysinfo::SystemInfo::collect(Some(TEST_SERIAL.to_string())).write_json(&mut expected_upload);
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

    let mut expected_json = Vec::new();
    sysinfo::SystemInfo::collect(Some(TEST_SERIAL.to_string())).write_json(&mut expected_json);
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
            serial: Some(TEST_SERIAL.to_string()),
            serial_source: Some("custom".to_string()),
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

fn dummy_hcd_test_lock() -> MutexGuard<'static, ()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
        .lock()
        .expect("dummy_hcd test lock poisoned")
}

struct GadgetServer {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<Result<(), String>>>,
}

impl GadgetServer {
    fn spawn(config: GadgetConfig) -> Result<Self, Box<dyn Error>> {
        let stop = Arc::new(AtomicBool::new(false));
        let stop_thread = Arc::clone(&stop);

        let thread = thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .map_err(|err| err.to_string())?;

            runtime.block_on(async move {
                gadget::serve_until(config, async move {
                    while !stop_thread.load(Ordering::Relaxed) {
                        tokio::time::sleep(Duration::from_millis(20)).await;
                    }
                })
                .await
                .map_err(|err| err.to_string())
            })
        });

        Ok(Self {
            stop,
            thread: Some(thread),
        })
    }

    fn shutdown(mut self) -> Result<(), Box<dyn Error>> {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            let result = thread
                .join()
                .map_err(|_| io::Error::other("gadget server thread panicked"))?;
            result.map_err(io::Error::other)?;
        }
        Ok(())
    }
}

impl Drop for GadgetServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

struct TestEnvironment {
    root: PathBuf,
    output_path: PathBuf,
    previous_path: Option<OsString>,
    previous_output: Option<OsString>,
}

impl TestEnvironment {
    #[allow(unsafe_code)]
    fn new() -> Result<Self, Box<dyn Error>> {
        let unique = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
        let root = std::env::temp_dir().join(format!("summit-usbgadget-hcd-dummy-{unique}"));
        fs::create_dir_all(&root)?;

        write_executable(
            &root.join("boot-rootfs.sh"),
            "#!/bin/sh\nrootDevType=initramfs\nbootside=a\nbaseHwPartNumber=test-hw\ngetSide() { return 0; }\ngetBaseHwPartNumber() { return 0; }\n",
        )?;
        write_executable(
            &root.join("fw_update"),
            "#!/bin/sh\ncat > \"$SUMMIT_USBGADGET_TEST_OUTPUT\"\n",
        )?;

        let previous_path = std::env::var_os("PATH");
        let previous_output = std::env::var_os("SUMMIT_USBGADGET_TEST_OUTPUT");

        let mut new_path = OsString::from(root.as_os_str());
        new_path.push(":");
        new_path.push(previous_path.clone().unwrap_or_default());
        let output_path = root.join("output.bin");
        // SAFETY: test process is single-threaded at this point; no concurrent env access.
        unsafe {
            std::env::set_var("PATH", new_path);
            std::env::set_var("SUMMIT_USBGADGET_TEST_OUTPUT", &output_path);
        }

        Ok(Self {
            root,
            output_path,
            previous_path,
            previous_output,
        })
    }

    fn set_output(&self, file_name: &str) -> io::Result<()> {
        let output_path = self.root.join(file_name);
        if output_path.exists() {
            fs::remove_file(&output_path)?;
        }
        // SAFETY: test process is single-threaded at this point; no concurrent env access.
        #[allow(unsafe_code)]
        unsafe {
            std::env::set_var("SUMMIT_USBGADGET_TEST_OUTPUT", &output_path);
        }
        Ok(())
    }

    fn read_output(&self) -> io::Result<Vec<u8>> {
        let output = std::env::var_os("SUMMIT_USBGADGET_TEST_OUTPUT")
            .map(PathBuf::from)
            .unwrap_or_else(|| self.output_path.clone());
        fs::read(output)
    }
}

impl Drop for TestEnvironment {
    #[allow(unsafe_code)]
    fn drop(&mut self) {
        // SAFETY: test process is single-threaded at this point; no concurrent env access.
        unsafe {
            match &self.previous_path {
                Some(value) => std::env::set_var("PATH", value),
                None => std::env::remove_var("PATH"),
            }
            match &self.previous_output {
                Some(value) => std::env::set_var("SUMMIT_USBGADGET_TEST_OUTPUT", value),
                None => std::env::remove_var("SUMMIT_USBGADGET_TEST_OUTPUT"),
            }
        }
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn write_executable(path: &Path, content: &str) -> io::Result<()> {
    fs::write(path, content)?;
    let mut permissions = fs::metadata(path)?.permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(path, permissions)
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
            USB_TIMEOUT,
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
            USB_TIMEOUT,
        )
        .wait()?;
    Ok(())
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

fn read_exact(reader: &mut impl Read, len: usize) -> io::Result<Vec<u8>> {
    let mut buf = vec![0u8; len];
    reader.read_exact(&mut buf)?;
    Ok(buf)
}

fn parse_hex_len(bytes: &[u8]) -> io::Result<usize> {
    let text = std::str::from_utf8(bytes).map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))?;
    usize::from_str_radix(text, 16).map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))
}

fn poll_until<F>(timeout: Duration, mut check: F) -> io::Result<()>
where
    F: FnMut() -> io::Result<bool>,
{
    let deadline = Instant::now() + timeout;
    loop {
        if check()? {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(io::Error::new(io::ErrorKind::TimedOut, "timed out waiting for USB state"));
        }
        thread::sleep(Duration::from_millis(50));
    }
}

fn wait_for_usb_device(vendor: u16, product: u16, timeout: Duration) -> io::Result<PathBuf> {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(path) = find_usb_device(vendor, product)? {
            return Ok(path);
        }
        if Instant::now() >= deadline {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                format!("USB device {:04x}:{:04x} did not enumerate within {:?}", vendor, product, timeout),
            ));
        }
        thread::sleep(Duration::from_millis(100));
    }
}

fn wait_for_usb_device_removal(vendor: u16, product: u16, timeout: Duration) -> io::Result<()> {
    let deadline = Instant::now() + timeout;
    loop {
        if find_usb_device(vendor, product)?.is_none() {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                format!("USB device {:04x}:{:04x} remained present for longer than {:?}", vendor, product, timeout),
            ));
        }
        thread::sleep(Duration::from_millis(100));
    }
}

fn wait_for_host_device(
    vendor: u16,
    product: u16,
    timeout: Duration,
) -> Result<nusb::DeviceInfo, Box<dyn Error>> {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(device_info) = nusb::list_devices().wait()?.find(|device| {
            device.vendor_id() == vendor && device.product_id() == product
        }) {
            return Ok(device_info);
        }

        if Instant::now() >= deadline {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                format!("host did not observe USB device {:04x}:{:04x} within {:?}", vendor, product, timeout),
            )
            .into());
        }

        thread::sleep(Duration::from_millis(100));
    }
}

fn find_usb_device(vendor: u16, product: u16) -> io::Result<Option<PathBuf>> {
    let wanted_vendor = format!("{vendor:04x}");
    let wanted_product = format!("{product:04x}");

    for entry in fs::read_dir("/sys/bus/usb/devices")? {
        let path = entry?.path();

        let Ok(found_vendor) = read_trimmed(&path.join("idVendor")) else {
            continue;
        };
        if found_vendor != wanted_vendor {
            continue;
        }

        let Ok(found_product) = read_trimmed(&path.join("idProduct")) else {
            continue;
        };
        if found_product == wanted_product {
            return Ok(Some(path));
        }
    }

    Ok(None)
}

fn read_trimmed(path: &Path) -> io::Result<String> {
    Ok(fs::read_to_string(path)?.trim().to_string())
}
