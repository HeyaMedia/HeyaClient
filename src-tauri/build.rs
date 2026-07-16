fn main() {
    if std::env::var_os("CARGO_FEATURE_NATIVE_MPV").is_some() {
        if let Some(lib_dir) = std::env::var_os("HEYA_LIBMPV_DIR") {
            println!(
                "cargo:rustc-link-search=native={}",
                lib_dir.to_string_lossy()
            );
        } else {
            pkg_config::Config::new()
                .cargo_metadata(true)
                .probe("mpv")
                .expect(
                    "native-mpv needs a staged libmpv; set HEYA_LIBMPV_DIR or provide mpv.pc on the development build host",
                );
        }
    }

    let attributes = tauri_build::Attributes::new().plugin(
        "native-bridge",
        tauri_build::InlinedPlugin::new()
            .commands(&["native_audio_request", "native_playback_request"]),
    );
    tauri_build::try_build(attributes).expect("failed to build HeyaClient's Tauri context")
}
