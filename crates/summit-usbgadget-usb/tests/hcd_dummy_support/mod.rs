use std::error::Error;
use std::ffi::OsString;
use std::fs;
use std::io;
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

use summit_usbgadget_usb::config::GadgetConfig;
use summit_usbgadget_usb::gadget;

pub(crate) const TEST_MANUFACTURER: &str = "Ezurio";
pub(crate) const TEST_SERIAL: &str = "dummy-hcd-protocols";

pub(crate) fn dummy_hcd_test_lock() -> MutexGuard<'static, ()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
        .lock()
        .expect("dummy_hcd test lock poisoned")
}

pub(crate) struct GadgetServer {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<Result<(), String>>>,
}

impl GadgetServer {
    pub(crate) fn spawn(config: GadgetConfig) -> Result<Self, Box<dyn Error>> {
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

    pub(crate) fn shutdown(mut self) -> Result<(), Box<dyn Error>> {
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

pub(crate) struct TestEnvironment {
    root: PathBuf,
    output_path: PathBuf,
    previous_path: Option<OsString>,
    previous_output: Option<OsString>,
}

impl TestEnvironment {
    pub(crate) fn new() -> Result<Self, Box<dyn Error>> {
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
        std::env::set_var("PATH", new_path);

        let output_path = root.join("output.bin");
        std::env::set_var("SUMMIT_USBGADGET_TEST_OUTPUT", &output_path);

        Ok(Self {
            root,
            output_path,
            previous_path,
            previous_output,
        })
    }

    pub(crate) fn set_output(&self, file_name: &str) -> io::Result<()> {
        let output_path = self.root.join(file_name);
        if output_path.exists() {
            fs::remove_file(&output_path)?;
        }
        std::env::set_var("SUMMIT_USBGADGET_TEST_OUTPUT", &output_path);
        Ok(())
    }

    pub(crate) fn read_output(&self) -> io::Result<Vec<u8>> {
        let output = std::env::var_os("SUMMIT_USBGADGET_TEST_OUTPUT")
            .map(PathBuf::from)
            .unwrap_or_else(|| self.output_path.clone());
        fs::read(output)
    }
}

impl Drop for TestEnvironment {
    fn drop(&mut self) {
        match &self.previous_path {
            Some(value) => std::env::set_var("PATH", value),
            None => std::env::remove_var("PATH"),
        }
        match &self.previous_output {
            Some(value) => std::env::set_var("SUMMIT_USBGADGET_TEST_OUTPUT", value),
            None => std::env::remove_var("SUMMIT_USBGADGET_TEST_OUTPUT"),
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

pub(crate) fn wait_for_usb_device(vendor: u16, product: u16, timeout: Duration) -> io::Result<PathBuf> {
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

pub(crate) fn wait_for_usb_device_removal(vendor: u16, product: u16, timeout: Duration) -> io::Result<()> {
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

pub(crate) fn read_trimmed(path: &Path) -> io::Result<String> {
    Ok(fs::read_to_string(path)?.trim().to_string())
}