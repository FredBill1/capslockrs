fn main() {
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }

    println!("cargo:rerun-if-changed=resources.rc");
    println!("cargo:rerun-if-changed=assets/icons/capslockrs.ico");
    println!("cargo:rerun-if-changed=assets/icons/capslockrs-paused.ico");

    embed_resource::compile("resources.rc", embed_resource::NONE)
        .manifest_optional()
        .expect("failed to compile Windows icon resources");
}
