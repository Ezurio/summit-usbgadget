# summit-usbgadget

A runtime-configurable composite USB gadget for Linux, written in Rust on top
of the [`usb-gadget`](https://crates.io/crates/usb-gadget) crate. The set of
functions is described by a configuration file, so the gadget can be recomposed
without recompiling.

Supported functions:

- **CDC ACM / generic serial port** (kernel gadget driver),
- **CDC network interface** — ECM, ECM subset, EEM, NCM, or RNDIS,
- **mass-storage device** (MSD),
- a **custom USB DFU 1.1 interface** implemented in user space. Received
  firmware is streamed directly into the **SWUpdate** IPC interface (via the
  `swupdate-ipc` crate) or written to a file; firmware uploads are served from
  a file.

## Requirements

- A Linux device with a USB device controller (UDC).
- Kernel options: `CONFIG_USB_GADGET`, `CONFIG_USB_CONFIGFS`,
  `CONFIG_USB_CONFIGFS_F_FS` (FunctionFS, for the DFU function), plus the option
  for each configured function, e.g. `CONFIG_USB_CONFIGFS_ACM` (serial),
  `CONFIG_USB_CONFIGFS_NCM` (network), `CONFIG_USB_CONFIGFS_MASS_STORAGE`.
- The DFU functional descriptor support in FunctionFS requires kernel 6.12 or
  later, unless your kernel has backported support.
- `configfs` mounted and root privileges to configure gadgets.
- For SWUpdate downloads, a running SWUpdate daemon with its IPC socket
  available.

## Build

```sh
cargo build --release
```

## Run

The gadget composition is read from a configuration file. Provide the path as
the first argument, via the `SUMMIT_USB_GADGET_CONFIG` environment variable, or fall
back to the default `/etc/summit-usbgadget.toml`:

```sh
sudo ./target/release/summit-usbgadget /etc/summit-usbgadget.toml
```

`RUST_LOG` controls log verbosity (`error`/`warn`/`info`/`debug`/`trace`).

## Configuration file

The configuration uses the same TOML schema as the upstream `usb-gadget` CLI
tool (a `[device]` table plus one or more `[[config]]` tables, each containing
`[[config.function]]` entries tagged by `type`), with additional `dfu` and
`fbk` function types serviced by this daemon. Configurations are therefore compatible
with the `usb-gadget` tool for the shared function types (`serial`, `net`,
`msd`). See [usb-gadget2.toml](usb-gadget2.toml) for a complete example.

```toml
name = "composite"
# udc = "11401000.usb"        # optional; defaults to the first available UDC

[device]
vendor = 0x1d50
product = 0x6089
manufacturer = "Ezurio"
product_name = "Composite DFU gadget"
serial = "0001"
class = 0xef                  # miscellaneous device (uses IADs)
sub_class = 2
protocol = 1

[[config]]
description = "composite"

[[config.function]]
type = "serial"
class = "acm"                 # acm | generic

[[config.function]]
type = "net"
class = "ncm"                 # ecm | ecm_subset | eem | ncm | rndis

[[config.function]]
type = "dfu"
download = "swupdate"         # swupdate | file:/path/to/output
transfer_size = 4096
poll_timeout_ms = 10
# software_set = "main"
# image_mode = "full"
# dry_run = false
# disable_store_swu = true
# timeout_secs = 120
# upload = "/var/tmp/readback.bin"
```

### Option reference

Top-level:

- `name`
- `udc` (`"name"`, `["a","b"]`, `"all"` / `"*"`)
- `os_descriptor.vendor_code`, `os_descriptor.qw_sign`, `os_descriptor.config`

`[device]`:

- required: `vendor`, `product`
- optional: `class`, `sub_class`, `protocol`
- optional strings: `manufacturer`, `product_name`, `serial`
- source selectors: `product_name_source` (`custom` / `model`),
  `serial_source` (`auto` / `custom` / `uboot_ethaddr` / `uboot_eth1addr`)

`[[config]]`:

- `description`, `max_power`, `self_powered`, `remote_wakeup`

`[[config.function]]` by `type`:

- `serial`: `class` (`acm` / `generic`), `console`
- `net`: `class`, `dev_addr`, `host_addr`, `qmult`,
  `os_compatible_id`, `os_sub_compatible_id`,
  `interface_class`, `interface_sub_class`, `interface_protocol`
- `msd`: `stall`, plus `[[config.function.lun]]` with
  `file`, `read_only`, `cdrom`, `no_fua`, `removable`, `inquiry_string`
- `dfu` and `fbk`: `download`, `upload`, `transfer_size`, `poll_timeout_ms`,
  `software_set`, `image_mode`, `dry_run`, `disable_store_swu`, `timeout_secs`

A `msd` function takes one or more `[[config.function.lun]]` tables with a
`file` backing path. Function types this service does not implement (e.g. `hid`,
`uac1`/`uac2`, `video`) are not accepted.

### Serial number

`device.serial_source` selects how the serial number is derived, mirroring the
summit-usbgadget script:

- `custom` (default when `serial` is set) — use `device.serial` verbatim.
- `auto` (default when `serial` is unset) — first available of `/etc/wifi_mac`,
  `/sys/devices/soc0/soc_uid`, `eth1`/`eth0` MAC (colons stripped), else
  `deadbeefdeadbeef`.
- `uboot_ethaddr` / `uboot_eth1addr` — the U-Boot environment variable read via
  `fw_printenv`, lowercased with colons stripped.

### Selecting controllers

`udc` chooses which USB device controllers to bind, and one gadget (with its
own DFU service task) is built per controller:

- omitted — bind the first controller that appears.
- `"name"` — bind that specific controller.
- `["a", "b"]` — bind each listed controller.
- `"all"` (or `"*"`) — bind every controller, including ones hotplugged later.

Controllers are discovered through **udev** events (via `tokio-udev`), not
polling, so the UDC kernel driver may be `modprobe`d after the service starts.
Existing controllers are enumerated at startup, then new ones are bound as their
udev `add` events on the `udc` subsystem arrive. This requires `libudev` and a
running udev daemon.

## Driving DFU

From a USB host, use `dfu-util`. Access to the USB device requires either
`sudo` or a udev rule granting permission to the current user (replace
`1fa3` / `0002` with the configured `vendor` / `product` values):

```
SUBSYSTEM=="usb", ATTR{idVendor}=="1fa3", ATTR{idProduct}=="0002", MODE="0664", GROUP="plugdev"
```

```sh
# Wrap the SWU in a DFU-suffixed host file to avoid dfu-util's suffix warning.
# The gadget strips the valid 16-byte DFU suffix before forwarding bytes into
# SWUpdate, so the original .swu payload reaches the target unchanged.
cp firmware.swu firmware.dfu
dfu-suffix -a firmware.dfu -v 0x1fa3 -p 0x0002

# Stream firmware to the gadget (forwarded into SWUpdate when download = swupdate)
sudo dfu-util -D firmware.dfu

# Read firmware back from the gadget (requires `upload` in the config)
sudo dfu-util -U readback.bin
```

For FBK / UUU uploads, keep using the raw `.swu` bundle; the DFU suffix wrapper
is only for the DFU transport.

The FBK function also exposes a small fastboot-compatible subset used by host
tools and recovery flows:

- `getvar:<name>` for the built-in variables implemented by the daemon
  (`version`, `max-download-size`, `max-fetch-size`, `product`, `serialno`,
  `is-userspace`, `all`, plus slot / partition queries for `update`, `swu`,
  `sysinfo`, and `sysinfo.json`)
- `fetch:sysinfo` and `fetch:sysinfo.json`
- `download:%08x` (and the legacy `download:`, `donwload:` spellings accepted
  by existing host flows)
- `flash:update` and `flash:swu`

The legacy FBK control commands `WOpen:<...>` and `Close` are also accepted for
UUU-style upload flows.

### Example i.MX8MM UUU recovery update

An example host-side `uuu` script is provided in `imx8mm-update.uuu`. It is an
invented template for boards that enter NXP USB serial download mode, boot a
temporary recovery Linux image, and then apply an SWUpdate bundle from the
recovery initramfs with `fw_update`.

The script expects these placeholder artifacts in the current directory:

- `_flash.bin` — bootable i.MX8MM flash image / U-Boot image.
- `_Image` — recovery kernel.
- `_board.dtb` — recovery device tree.
- `_initramfs.cpio.gz.uboot` — recovery initramfs containing `/linuxrc`, the
  fastboot-kernel (`FBK`) agent, and `fw_update`.
- `_update.swu` — SWUpdate bundle to install.

Run it with:

```sh
uuu imx8mm-update.uuu
```

this recovery flow uses `fw_update -x r -m complete`, which matches the
initramfs / pipe-mode update path described below. If you instead want to test
the DFU gadget itself, boot a recovery image that starts `summit-usbgadget`
and use `dfu-util -D firmware.swu` from the host.

### Device identity and system information

The device's name and serial number are exposed through the standard USB string
descriptors (`iManufacturer`/`iProduct`/`iSerialNumber`), set from the `[device]`
table and the `serial_source` rules, and shown by `dfu-util -l`.

For richer details, set `upload = "sysinfo"` on the DFU function. `DFU_UPLOAD`
then returns a `key=value` report gathered from the running system (the same
sources as the platform `boot-rootfs.sh`):

```text
model=...
serial=...
soc=...
memory_mb=...
hw_part_number=...
```

Read it from a host with `dfu-util -U sysinfo.txt`. Note that `upload` serves
either firmware read-back (a file path) or the system-information report
(`"sysinfo"`), not both.

When `download = "swupdate"`, each DFU block is forwarded to the SWUpdate daemon
over its control socket as it arrives — no local or temporary file is created.
By default `disable_store_swu` is set, so SWUpdate processes the stream directly
without persisting a copy of the SWU. The manifestation phase awaits a terminal
result on the SWUpdate progress interface, which is mapped to the DFU status
reported to the host.

#### A/B slot selection

SWUpdate writes the update to the correct slot. The target is a pure **runtime**
decision (the user has no choice), determined by running `boot-rootfs.sh`
(mirroring summit-rcm):

- **SD-card boot** → a full-disk update mode: `complete` (no side).
- **Any other device** → the inactive A/B slot:
  `"<image_mode>-<inactive_side>"`, where `inactive_side` is the
  opposite of the current boot side (default `a`).

`image_mode` (default `"full"`, used only in the A/B case) and `software_set`
(default `"stable"`) may be overridden via config; neither affects the side.

## Layout

- [src/main.rs](src/main.rs) — loads the configuration, discovers controllers
  via udev, binds a gadget per controller, and runs a DFU task for each.
- [src/config.rs](src/config.rs) — the TOML configuration schema.
- [src/sysinfo/](src/sysinfo) — boot context, serial derivation, and system-information retrieval.
- [src/udc.rs](src/udc.rs) — udev-based controller discovery and selection.
- [src/gadget.rs](src/gadget.rs) — builds and binds a composite gadget on a UDC.
- [src/dfu/](src/dfu) — the DFU 1.1 protocol module: state/status enumerations,
  configuration, firmware sinks (SWUpdate IPC or file), and the handler.
- [src/swupdate/](src/swupdate) — SWUpdate transport dispatch with split IPC and pipe backends.

