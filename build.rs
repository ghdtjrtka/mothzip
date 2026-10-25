fn main() {
    #[cfg(windows)]
    {
        let res_path = std::path::Path::new("assets/moth.res");
        if res_path.exists() {
            // Tell cargo to re-run if moth.res changes
            println!("cargo:rerun-if-changed=assets/moth.res");
            // Pass the compiled .res file directly to MSVC link.exe
            println!("cargo:rustc-link-arg=assets/moth.res");
        }
    }
}
