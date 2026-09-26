fn main() {
    #[cfg(target_os = "macos")]
    {
        cc::Build::new()
            .file("native/virtual_display.m")
            .flag("-fobjc-arc")
            .compile("airbridge_virtual_display");
        println!("cargo:rustc-link-lib=framework=Foundation");
        println!("cargo:rustc-link-lib=framework=CoreGraphics");
        println!("cargo:rustc-link-arg=-Wl,-rpath,/usr/lib/swift");
    }
}
