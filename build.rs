use std::env;
use std::process::Command;

fn main() {
    // Set the app name and bundle identifier based on the profile
    let profile = env::var("PROFILE").unwrap_or_else(|_| "debug".to_string());
    if profile == "debug" {
        println!("cargo:rustc-env=APP_NAME=Hobbes (Dev)");
        println!("cargo:rustc-env=BUNDLE_IDENTIFIER=ai.clearmirror.cai-hobbes-dev");
    } else {
        println!("cargo:rustc-env=APP_NAME=Hobbes");
        println!("cargo:rustc-env=BUNDLE_IDENTIFIER=ai.clearmirror.cai-hobbes");
    }

    // Run tailwindcss to build the CSS file.
    // Try local node_modules binary first, fall back to npx.
    // Skip gracefully if neither is available — output.css is committed to the repo.
    let tailwind_args = [
        "-i",
        "./assets/tailwind.css",
        "-o",
        "./assets/output.css",
        "--minify",
    ];

    let result = Command::new("./node_modules/.bin/tailwindcss")
        .args(tailwind_args)
        .status()
        .or_else(|_| {
            let npx = if cfg!(target_os = "windows") { "npx.cmd" } else { "npx" };
            Command::new(npx)
                .args(["tailwindcss"])
                .args(tailwind_args)
                .status()
        });

    match result {
        Ok(status) if status.success() => {}
        Ok(status) => {
            println!("cargo:warning=tailwindcss exited with status {}. Using existing output.css.", status);
        }
        Err(_) => {
            println!("cargo:warning=tailwindcss not found. Using existing output.css.");
        }
    }
}
