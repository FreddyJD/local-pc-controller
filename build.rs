fn main() {
    println!("cargo:rerun-if-changed=packaging/icons/localnet.ico");
    println!("cargo:rerun-if-changed=build.rs");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        winresource::WindowsResource::new()
            .set_icon("packaging/icons/localnet.ico")
            .set("ProductName", "Localnet")
            .set("FileDescription", "Localnet - Private LAN Desktop")
            .set("InternalName", "localnet")
            .set("OriginalFilename", "Localnet.exe")
            .compile()
            .expect("Failed to embed the Localnet Windows icon and version resource");
    }
}
