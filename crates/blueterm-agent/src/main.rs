//! blueterm-agent: offers a login console over the BLE Nordic UART Service.

#[cfg(target_os = "linux")]
mod config;
#[cfg(target_os = "linux")]
mod linux_main;
#[cfg(target_os = "linux")]
mod pair;
#[cfg(target_os = "linux")]
mod pty;
#[cfg(target_os = "linux")]
mod server;
#[cfg(target_os = "linux")]
mod session;

#[cfg(target_os = "linux")]
fn main() -> std::process::ExitCode {
    linux_main::main()
}

#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!("blueterm-agent only runs on Linux (it requires BlueZ).");
    std::process::exit(1);
}
