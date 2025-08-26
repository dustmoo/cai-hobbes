use std::process::Command;

fn main() {
    // Run tailwindcss to build the CSS file
    let status = Command::new("npx")
        .args(&[
            "tailwindcss",
            "-i",
            "./assets/tailwind.css",
            "-o",
            "./assets/output.css",
            "--minify"
        ])
        .status()
        .expect("failed to execute tailwindcss");

    if !status.success() {
        panic!("tailwindcss failed to build");
    }
}