fn main() {
    let output = std::process::Command::new("date")
        .args(["+%Y-%m-%d %H:%M:%S"])
        .output()
        .expect("failed to run date");
    let timestamp = String::from_utf8_lossy(&output.stdout).trim().to_string();
    println!("cargo:rustc-env=HM_BUILD_TIME={timestamp}");
}
