fn main() {
    // App icon + manifest (Per-Monitor V2 DPI awareness, Common Controls 6, UTF-8).
    println!("cargo:rerun-if-changed=assets/chop-chop.rc");
    println!("cargo:rerun-if-changed=assets/chop-chop.manifest");
    println!("cargo:rerun-if-changed=assets/chop-chop.ico");
    let target = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    if target == "windows" {
        embed_resource::compile("assets/chop-chop.rc", embed_resource::NONE).manifest_required().unwrap();
    }
}
