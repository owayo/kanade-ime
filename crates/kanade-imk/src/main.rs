fn main() {
    #[cfg(target_os = "macos")]
    {
        let register = std::env::args().any(|arg| arg == "--register-input-source");
        if register || std::env::args().any(|arg| arg == "--input-source-status") {
            let enable = register && std::env::args().any(|arg| arg == "--enable");
            match kanade_imk::registration::inspect(register, enable) {
                Ok(status) => println!(
                    "{}",
                    serde_json::to_string(&status).expect("registration status")
                ),
                Err(error) => {
                    eprintln!("kanade-imk: {error}");
                    std::process::exit(1);
                }
            }
        } else if let Err(error) = kanade_imk::native::run() {
            eprintln!("kanade-imk: {error}");
            std::process::exit(1);
        }
    }
    #[cfg(not(target_os = "macos"))]
    eprintln!("kanade-imk requires macOS");
}
