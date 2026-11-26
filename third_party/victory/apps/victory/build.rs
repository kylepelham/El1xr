//! (Neptune) The diagnostics build exports the two symbols by which the D3D12 loader finds a
//! redistributable runtime and debug layer beside the executable (`D3D12\`): the debug layer
//! Windows installs is older than its D3D12 runtime on some PCs, and a device is then refused.
fn main() {
    println!("cargo:rerun-if-env-changed=VICTORY_AGILITY_SDK");
    println!("cargo:rustc-check-cfg=cfg(victory_agility_sdk)");
    if std::env::var_os("CARGO_FEATURE_DIAGNOSTICS").is_some()
        && std::env::var_os("VICTORY_AGILITY_SDK").is_some_and(|v| v == "1")
    {
        println!("cargo:rustc-cfg=victory_agility_sdk");
        println!("cargo:rustc-link-arg-bins=/EXPORT:D3D12SDKVersion,DATA");
        println!("cargo:rustc-link-arg-bins=/EXPORT:D3D12SDKPath,DATA");
    }
}
