//! Windows: embed the app icon (assets/scopewerk.rc) into Scopewerk.exe.

fn main() {
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        println!("cargo:rerun-if-changed=assets/scopewerk.rc");
        println!("cargo:rerun-if-changed=assets/Scopewerk.ico");
        embed_resource::compile(
            "assets/scopewerk.rc",
            embed_resource::ParamsIncludeDirs(["assets"]),
        )
        .manifest_optional()
        .expect("compiling the Windows resources");
    }
}
