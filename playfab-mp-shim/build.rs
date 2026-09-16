use std::time::{SystemTime, UNIX_EPOCH};

fn main() {
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    println!("cargo:rustc-env=BUILD_STAMP={stamp}");
}
