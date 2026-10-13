#![cfg(target_os = "macos")]

#[test]
fn development_runner_preserves_cli_arguments() {
    let output = std::process::Command::new("sh")
        .arg("packaging/macos/run-dev.sh")
        .arg(env!("CARGO_BIN_EXE_spotifast"))
        .arg("--version")
        .output()
        .expect("run macOS development client");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        String::from_utf8(output.stdout)
            .unwrap()
            .contains(env!("CARGO_PKG_VERSION"))
    );
}

#[test]
fn regular_client_bundle_has_the_spatial_control_identity() {
    let app = std::path::Path::new(env!("CARGO_BIN_EXE_spotifast"))
        .parent()
        .unwrap()
        .join("Spotifast-packaging-test.app");
    let output = std::process::Command::new("bash")
        .arg("packaging/macos/bundle.sh")
        .arg(env!("CARGO_BIN_EXE_spotifast"))
        .arg(&app)
        .arg(env!("CARGO_PKG_VERSION"))
        .env_remove("CODESIGN_IDENTITY")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let plist = std::fs::read_to_string(app.join("Contents/Info.plist")).unwrap();
    assert!(
        plist.contains("<key>CFBundleIdentifier</key><string>rocks.spotifast.Spotifast</string>")
    );
    assert!(plist.contains("<key>CFBundleName</key><string>Spotifast</string>"));
    std::fs::remove_dir_all(app).unwrap();
}
