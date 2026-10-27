//! Exercise the real native window controller without CUDA.
#![cfg(all(feature = "gpu-profiles", target_os = "linux"))]
use std::process::Command;

#[test]
fn native_controller_windows_failure_and_shutdown() {
    let path = std::env::temp_dir().join(format!("mars-gpu-capture-{}", std::process::id()));
    assert!(Command::new("cc")
        .args([
            "-std=c11",
            "-Wall",
            "-Wextra",
            "-Werror",
            "-pthread",
            "native/gpu_capture_test.c",
            "native/gpu_capture.c",
            "-o"
        ])
        .arg(&path)
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .status()
        .unwrap()
        .success());
    for mode in ["continuous", "full", "periodic", "long", "failure"] {
        assert!(
            Command::new(&path).arg(mode).status().unwrap().success(),
            "{mode}"
        );
    }
    std::fs::remove_file(path).unwrap();
}
