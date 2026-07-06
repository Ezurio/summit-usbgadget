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

In VS Code, the workspace also provides a `build` task.

## Testing with dummy_hcd

The ignored integration tests in the `summit-usbgadget-dfu`,
`summit-usbgadget-fastboot-usb`, and `summit-usbgadget-usb` crates exercise those
protocols against Linux's virtual USB loopback controller. This repo does not
vendor the kernel module; build and load `dummy_hcd` separately.

One supported path is the out-of-tree module published by
[`xairy/raw-gadget`](https://github.com/xairy/raw-gadget), which includes a
copy of the `dummy_hcd` module source and helper scripts:

```sh
git clone https://github.com/xairy/raw-gadget
cd raw-gadget/dummy_hcd

# Match the module source to the target kernel when needed.
./update.sh 6.12

make
sudo ./insmod.sh
```

That requires the matching kernel headers for the running kernel, typically via
your distro's `linux-headers-$(uname -r)` package.

After `dummy_hcd` is loaded and `configfs` is mounted, run the ignored tests as
root:

```sh
sudo cargo test -p summit-usbgadget-dfu --test hcd_dummy -- --ignored
sudo cargo test -p summit-usbgadget-fastboot-usb --test hcd_dummy -- --ignored
sudo cargo test -p summit-usbgadget-usb --test hcd_dummy_protocols -- --ignored
```

The test currently assumes a Linux host with root privileges, `configfs`, and a
working `dummy_hcd` UDC. Unload the module afterwards if you no longer need the
virtual controller.

## Run

The gadget composition is read from a configuration file. Provide the path as
the first argument, via the `SUMMIT_USB_GADGET_CONFIG` environment variable, or fall
back to the default `/etc/summit-usbgadget.toml`:

```sh
sudo ./target/release/summit-usbgadget /etc/summit-usbgadget.toml
```

In VS Code, the workspace `run` task builds the release binary first and then
starts it with the repository's `summit-usbgadget.toml` example config.

`RUST_LOG` controls log verbosity (`error`/`warn`/`info`/`debug`/`trace`).

### Socket SWUpdate source

The daemon can also accept firmware over a plain TCP or TLS socket, independent
of USB. It is compiled in with the `socket` cargo feature (or `socket-tls` to
include OpenSSL TLS support) and runs as a startup service inside the main
`summit-usbgadget` binary — there is no separate executable:

```sh
cargo build --release --features socket-tls
```

Configure it with a top-level `[socket_source]` section in the shared
`summit-usbgadget.toml`. When a `[socket_source.tls]` sub-section is present —
and the binary was built with TLS support — the listener serves TLS; otherwise
(or when built without TLS support, in which case the section is ignored) it
accepts plain TCP. It accepts one incoming update stream at a time and forwards
it into SWUpdate.

### Fastboot over TCP

The daemon can also serve the same fastboot flow as the USB `fastboot-usb` function over
a TCP socket, using the AOSP fastboot "TCP Protocol v1" framing (a mutual `FB01`
handshake followed by 8-byte length-prefixed packets). It is compiled in with
the `fastboot-tcp` cargo feature and runs as a startup service inside the main
`summit-usbgadget` binary:

```sh
cargo build --release --features fastboot-tcp
```

Configure it with a top-level `[fastboot_tcp]` section in the shared
`summit-usbgadget.toml` (listen `address` defaults to `0.0.0.0:5554`, the
fastboot TCP port). It serves one host at a time and offers the same
getvar/fetch/download/flash update flow into SWUpdate, so a host connects with:

```sh
fastboot -s tcp:<device-ip> getvar:version
fastboot -s tcp:<device-ip> flash update firmware.swu
```

## Configuration file

The configuration uses the same TOML schema as the upstream `usb-gadget` CLI
tool (a `[device]` table plus one or more `[[config]]` tables, each containing
`[[config.function]]` entries tagged by `type`), with additional `dfu` and
`fastboot-usb` function types serviced by this daemon. Configurations are therefore compatible
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
download = "swupdate"         # swupdate | socket
transfer_size = 4096
poll_timeout_ms = 10
# software_set = "main"
# image_mode = "full"
# dry_run = false
# disable_store_swu = true
# timeout_secs = 120

# Stream to an incoming TCP or TLS socket instead of local SWUpdate.
# download = "socket"
# [config.function.download_socket]
# address = "0.0.0.0:8443"
# accept_timeout_secs = 15
# shutdown_timeout_secs = 15
# [config.function.download_socket.tls]
# server_cert = "/etc/ssl/certs/update-server.pem"
# server_key = "/etc/ssl/private/update-server.key"
# request_client_cert = false
# ca_cert = "/etc/ssl/certs/update-ca.pem"
# ignore_expiration = true
# fips = false
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
- `dfu` and `fastboot-usb`: `download`, `upload`, `transfer_size`, `poll_timeout_ms`,
  `software_set`, `image_mode`, `dry_run`, `disable_store_swu`, `timeout_secs`,
  optional `download_socket.address`, `download_socket.accept_timeout_secs`,
  `download_socket.shutdown_timeout_secs`, and `download_socket.tls.*`

`download_socket.tls` accepts:

- `server_cert`, `server_key` — PEM server certificate chain and private key
  presented by the listener.
- `request_client_cert` — request and validate an incoming client certificate
  against `ca_cert` (mutual TLS). Requires `ca_cert` to be set.
- `ca_cert` — PEM CA certificate that incoming client certificates are validated
  against when `request_client_cert` is enabled.
- `ignore_expiration` — ignore client-certificate validity timestamps during
  validation (default `true`).
- `fips` — enable OpenSSL 3 FIPS mode for TLS operations. This requires an
  OpenSSL 3 build with the FIPS provider installed.

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

For fastboot-usb / UUU uploads, keep using the raw `.swu` bundle; the DFU suffix wrapper
is only for the DFU transport.

The fastboot-usb function also exposes a small fastboot-compatible subset used by host
tools and recovery flows:

- `getvar:<name>` for the built-in variables implemented by the daemon
  (`version`, `max-download-size`, `max-fetch-size`, `product`, `serialno`,
  `is-userspace`, `all`, plus slot / partition queries for `update`, `swu`,
  `sysinfo`, and `sysinfo.json`)
- `fetch:sysinfo` and `fetch:sysinfo.json`
- `download:%08x` (and the legacy `download:`, `donwload:` spellings accepted
  by existing host flows)
- `flash:update` and `flash:swu`

The legacy fastboot-usb control commands `WOpen:<...>` and `Close` are also accepted for
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
  fastboot-kernel (`fastboot-usb`) agent, and `fw_update`.
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

When `download = "socket"`, each DFU or fastboot-usb block is streamed to one incoming
connection accepted on the configured local socket instead. Without a
`download_socket.tls` table the session is a plain TCP stream. Adding
`download_socket.tls` switches to OpenSSL-backed server-side TLS, with a server
certificate/key and optional client-certificate validation against a provided
PEM CA. The socket transport completes when the stream
is cleanly shut down, and it does not trigger the local SWUpdate reboot path.

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
- [src/swupdate/](src/swupdate) — update transport dispatch with local SWUpdate IPC/pipe and remote socket backends.
