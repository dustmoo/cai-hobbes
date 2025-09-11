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

    // Run tailwindcss to build the CSS file
    let status = Command::new("npx")
        .args(&[
            "tailwindcss",
            "-i",
            "./assets/tailwind.css",
            "-o",
            "./assets/output.css",
            "--minify",
        ])
        .status()
        .expect("failed to execute tailwindcss");

    if !status.success() {
        panic!("tailwindcss failed to build");
    }
}