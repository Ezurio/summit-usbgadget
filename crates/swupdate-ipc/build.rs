//
// SPDX-License-Identifier: LicenseRef-Ezurio-Clause
//

fn main() {
    println!("cargo:rustc-check-cfg=cfg(swupdate_progress_msg_packed)");
    println!("cargo:rerun-if-env-changed=SWUPDATE_PROGRESS_MSG_LAYOUT");
    println!("cargo:rerun-if-env-changed=SWUPDATE_INCLUDE_DIR");
    println!("cargo:rerun-if-changed=src/swupdate_sys.rs");

    let packed = match std::env::var("SWUPDATE_PROGRESS_MSG_LAYOUT") {
        Ok(layout) => match layout.as_str() {
            "packed" => true,
            "unpacked" => false,
            _ => panic!(
                "SWUPDATE_PROGRESS_MSG_LAYOUT must be `packed` or `unpacked`, got {layout:?}"
            ),
        },
        Err(_) => detect_packed_progress_msg().unwrap_or_else(|| {
            println!(
                "cargo:warning=SWUpdate progress_ipc.h was not found; defaulting ProgressMsg to packed"
            );
            true
        }),
    };
    if packed {
        println!("cargo:rustc-cfg=swupdate_progress_msg_packed");
    }

    let mut bindings = std::fs::read_to_string("src/swupdate_sys.rs")
        .expect("SWUpdate protocol definitions should be readable");
    if !packed {
        let marker = "#[repr(C, packed)]\n#[derive(Debug, Copy, Clone, PartialEq, Eq)]\npub struct ProgressMsg";
        assert!(
            bindings.contains(marker),
            "ProgressMsg representation marker should exist"
        );
        bindings = bindings.replacen(
            marker,
            "#[repr(C)]\n#[derive(Debug, Copy, Clone, PartialEq, Eq)]\npub struct ProgressMsg",
            1,
        );
    }

    let output = std::path::PathBuf::from(std::env::var("OUT_DIR").expect("OUT_DIR set by cargo"))
        .join("swupdate_sys.rs");
    std::fs::write(output, bindings).expect("selected SWUpdate protocol definitions should be writable");
}

fn detect_packed_progress_msg() -> Option<bool> {
    let header = find_progress_header()?;
    println!("cargo:rerun-if-changed={}", header.display());

    let source = std::fs::read_to_string(&header).unwrap_or_else(|error| {
        panic!(
            "failed to read SWUpdate progress header {}: {error}",
            header.display()
        )
    });
    let packed = progress_msg_is_packed(&source).unwrap_or_else(|| {
        panic!(
            "could not find a complete `struct progress_msg` declaration in {}",
            header.display()
        )
    });
    println!(
        "cargo:info=selected {} ProgressMsg layout from {}",
        if packed { "packed" } else { "unpacked" },
        header.display()
    );
    Some(packed)
}

fn find_progress_header() -> Option<std::path::PathBuf> {
    let header = std::path::PathBuf::from(std::env::var("SWUPDATE_INCLUDE_DIR").ok()?)
        .join("progress_ipc.h");
    header.is_file().then_some(header)
}

fn progress_msg_is_packed(source: &str) -> Option<bool> {
    let declaration = &source[source.find("struct progress_msg")?..];
    let body_start = declaration.find('{')?;
    let mut nesting = 0;
    let mut body_end = None;
    for (index, character) in declaration[body_start..].char_indices() {
        match character {
            '{' => nesting += 1,
            '}' => {
                nesting -= 1;
                if nesting == 0 {
                    body_end = Some(body_start + index);
                    break;
                }
            }
            _ => {}
        }
    }
    let suffix = &declaration[body_end?..];
    let terminator = suffix.find(';')?;
    Some(suffix[..terminator].contains("packed"))
}
