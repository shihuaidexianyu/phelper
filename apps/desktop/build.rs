fn main() {
    println!("cargo:rerun-if-changed=assets/phelper.ico");
    println!("cargo:rerun-if-changed=phelper.rc");
    embed_resource::compile("phelper.rc", embed_resource::NONE)
        .manifest_optional()
        .expect("embed phelper Windows icon");
}
