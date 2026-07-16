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

    tauri_build::build()
}
