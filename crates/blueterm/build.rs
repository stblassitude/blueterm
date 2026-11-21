//! On macOS, embed an Info.plist into the binary. CoreBluetooth requires
//! `NSBluetoothAlwaysUsageDescription`, even for command-line tools.

fn main() {
    let template = "../../packaging/macos/Info.plist.in";
    println!("cargo:rerun-if-changed={template}");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("macos") {
        return;
    }
    let version = std::env::var("CARGO_PKG_VERSION").unwrap();
    let plist = std::fs::read_to_string(template).unwrap().replace("@VERSION@", &version);
    let out = std::path::Path::new(&std::env::var("OUT_DIR").unwrap()).join("Info.plist");
    std::fs::write(&out, plist).unwrap();
    println!("cargo:rustc-link-arg-bins=-Wl,-sectcreate,__TEXT,__info_plist,{}", out.display());
}
