//! GPUI 0.2.2 links xkbcommon-x11 even with only its Wayland backend enabled.
//! Some hosts have the runtime SONAME but no development linker symlink.
use std::{env, path::PathBuf, process::Command};

fn compiler_library(name: &str) -> Option<PathBuf> {
    let output = Command::new(env::var_os("CC").unwrap_or_else(|| "cc".into()))
        .arg(format!("-print-file-name={name}"))
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let path = PathBuf::from(String::from_utf8(output.stdout).ok()?.trim());
    path.is_absolute().then_some(path).filter(|p| p.is_file())
}

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-env-changed=CC");
    println!("cargo:rerun-if-env-changed=LIBRARY_PATH");
    if env::var_os("CARGO_FEATURE_GUI").is_none()
        || env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("linux")
        || env::var_os("HOST") != env::var_os("TARGET")
    {
        return;
    }
    if compiler_library("libxkbcommon-x11.so").is_some() {
        return;
    }
    let Some(runtime) = compiler_library("libxkbcommon-x11.so.0") else {
        println!(
            "cargo:warning=Install the xkbcommon-x11 development library (libxkbcommon-x11-devel on Fedora, libxkbcommon-x11-dev on Debian/Ubuntu)"
        );
        return;
    };
    println!("cargo:rerun-if-changed={}", runtime.display());
    #[cfg(unix)]
    {
        let directory =
            PathBuf::from(env::var_os("OUT_DIR").expect("Cargo OUT_DIR")).join("native");
        std::fs::create_dir_all(&directory).expect("Create native linker directory");
        let link = directory.join("libxkbcommon-x11.so");
        if link.symlink_metadata().is_ok() {
            std::fs::remove_file(&link).expect("Refresh native linker symlink");
        }
        std::os::unix::fs::symlink(runtime, link).expect("Create xkbcommon-x11 linker symlink");
        println!("cargo:rustc-link-search=native={}", directory.display());
    }
}
