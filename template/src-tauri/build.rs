use std::fs;
use std::path::Path;

fn main() {
    let cache = Path::new("..").join("installer").join("cache");
    let _ = fs::create_dir_all(&cache);
    for name in [
        "docker-compose.exe",
        "podman-installer.msi",
        "LICENSE-podman",
        "LICENSE-compose",
    ] {
        let path = cache.join(name);
        println!("cargo:rerun-if-changed={}", path.display());
        if !path.exists() {
            let _ = fs::write(&path, b"");
        }
    }
    tauri_build::build();
}
