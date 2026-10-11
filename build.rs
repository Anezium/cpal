fn main() {
    println!("cargo:rerun-if-env-changed=VITASDK");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("vita") {
        // A static #[link] library must be found while creating the rlib, before
        // the final GCC link. cargo-vita's linker environment alone is not enough.
        let sdk = std::env::var_os("VITASDK").expect("VITASDK is required for the Vita audio host");
        let lib = std::path::PathBuf::from(sdk).join("arm-vita-eabi/lib");
        println!("cargo:rustc-link-search=native={}", lib.display());
    }
}
