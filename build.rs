fn main() {
    if std::env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("msvc") {
        // Windows "N" editions ship without Media Foundation. Delay-loading
        // mfplat.dll lets the app start and explain how to install it,
        // instead of Windows refusing to launch the exe.
        println!("cargo:rustc-link-arg-bins=/DELAYLOAD:mfplat.dll");
        println!("cargo:rustc-link-arg-bins=delayimp.lib");
    }
}
