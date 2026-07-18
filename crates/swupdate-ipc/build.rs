//
// SPDX-License-Identifier: LicenseRef-Ezurio-Clause
//

//! Generates the wire protocol types in `src/proto.rs` straight from the real
//! SWUpdate C headers via bindgen, instead of hand-mirroring them. This is
//! what should have caught the `progress_msg` packed-layout drift that
//! previously went unnoticed in a hand-written copy of `struct progress_msg`
//! (see the doc comment on `ProgressMsg` in `src/proto.rs`).
//!
//! When `SWUPDATE_INCLUDE_DIR` is set, bindings are regenerated from its
//! headers. Otherwise this copies the checked-in default bindings generated
//! from the supported SWUpdate headers, so consumers do not need the dev
//! package merely to build the client.
//!
//! Header discovery: `bindgen::Builder::header()` requires a path that
//! actually resolves on disk — it does NOT fall back to clang's `-I` search
//! for its own primary inputs (only for headers `#include`d from within
//! them). So `SWUPDATE_INCLUDE_DIR` is joined onto each filename directly,
//! and also passed as `-I` so those two files can `#include` anything else
//! (e.g. `swupdate_status.h`) from the same directory.

#[derive(Debug)]
struct RustNames;

impl bindgen::callbacks::ParseCallbacks for RustNames {
    fn item_name(&self, item: bindgen::callbacks::ItemInfo<'_>) -> Option<String> {
        let name = match item.name {
            "RECOVERY_STATUS" => "RecoveryStatus",
            "ipc_message" => "IpcMessage",
            "msgdata" => "MsgData",
            "msgtype" => "MsgType",
            "progress_connect_ack" => "ProgressConnectAck",
            "progress_msg" => "ProgressMsg",
            "run_type" => "RunType",
            "sourcetype" => "SourceType",
            "swupdate_request" => "SwupdateRequest",
            _ => return None,
        };
        Some(name.into())
    }
}

fn main() {
    println!("cargo:rerun-if-env-changed=SWUPDATE_INCLUDE_DIR");
    println!("cargo:rerun-if-changed=src/swupdate_sys.rs");

    let out_dir = std::path::PathBuf::from(std::env::var("OUT_DIR").expect("OUT_DIR set by cargo"));
    let output = out_dir.join("swupdate_sys.rs");
    let Ok(include_dir) = std::env::var("SWUPDATE_INCLUDE_DIR") else {
        std::fs::copy("src/swupdate_sys.rs", output)
            .expect("default SWUpdate bindings should be readable and writable");
        return;
    };

    let include_dir = std::path::Path::new(&include_dir);

    let bindings = bindgen::Builder::default()
        .header(
            include_dir
                .join("progress_ipc.h")
                .to_string_lossy()
                .into_owned(),
        )
        .header(
            include_dir
                .join("network_ipc.h")
                .to_string_lossy()
                .into_owned(),
        )
        .clang_arg(format!("-I{}", include_dir.display()))
        .allowlist_type("progress_msg")
        .allowlist_type("progress_connect_ack")
        .allowlist_type("ipc_message")
        .allowlist_type("msgdata")
        .allowlist_type("swupdate_request")
        .allowlist_type("msgtype")
        .allowlist_type("run_type")
        .allowlist_type("RECOVERY_STATUS")
        .allowlist_type("sourcetype")
        .allowlist_var("PROGRESS_API_.*")
        .allowlist_var("IPC_MAGIC")
        .allowlist_var("SWUPDATE_API_VERSION")
        .allowlist_var("CMD_.*")
        .parse_callbacks(Box::new(RustNames))
        .default_enum_style(bindgen::EnumVariation::Rust {
            non_exhaustive: false,
        })
        .derive_copy(true)
        .derive_debug(true)
        .derive_eq(true)
        .generate()
        .expect(
            "bindgen failed to parse the SWUpdate headers in SWUPDATE_INCLUDE_DIR; check that \
             progress_ipc.h and network_ipc.h are present there",
        );

    bindings
        .write_to_file(output)
        .expect("generated bindings should be writable");
}
