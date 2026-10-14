use std::env;
use std::fs;
use std::path::Path;
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

    // Keep assets/mermaid.min.js in sync with node_modules before anything
    // downstream (`include_str!` in src/components/mermaid.rs) needs it.
    // The file itself is gitignored — this is what reproduces it, both
    // locally and in CI (which runs `npm ci` before building — see
    // .github/workflows/ci.yml). `npm run sync:mermaid` does the same copy
    // by hand for refreshing the pinned version outside of a build.
    sync_mermaid_bundle();

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

/// Copy `node_modules/mermaid/dist/mermaid.min.js` -> `assets/mermaid.min.js`
/// when the source exists and the destination is missing or older, so the
/// 5MB+ bundle never has to live in git history but is still present for
/// `include_str!` at compile time.
///
/// If node_modules is absent AND a previously-copied asset is also missing
/// (e.g. a fresh checkout that hasn't run `npm ci` yet), write a small stub
/// instead of failing the build — same degrade-with-a-warning behaviour as
/// the tailwindcss step above. The stub defines a `window.mermaid` shim
/// whose `parse`/`render` reject with a clear message, so the diagram
/// component's existing error path shows the source plus that message
/// (honest degradation) instead of a hard build failure.
fn sync_mermaid_bundle() {
    let src = Path::new("node_modules/mermaid/dist/mermaid.min.js");
    let dest = Path::new("assets/mermaid.min.js");

    // Re-run this build script if the pinned copy in node_modules changes
    // (e.g. `npm ci` bumps the mermaid version) even though the source path
    // itself is gitignored.
    println!("cargo:rerun-if-changed={}", src.display());

    if src.exists() {
        let dest_is_missing_or_older = match (fs::metadata(dest), fs::metadata(src)) {
            (Ok(dest_meta), Ok(src_meta)) => {
                match (dest_meta.modified(), src_meta.modified()) {
                    (Ok(dest_t), Ok(src_t)) => src_t > dest_t,
                    _ => true,
                }
            }
            _ => true, // dest missing (or unreadable) -> copy
        };

        if dest_is_missing_or_older && let Err(e) = fs::copy(src, dest) {
            println!(
                "cargo:warning=failed to copy mermaid.min.js from node_modules ({e}). Using existing assets/mermaid.min.js if present."
            );
        }
    }

    if !dest.exists() {
        let stub = r#"(function () {
    var MSG = "mermaid bundle missing — run npm ci && npm run sync:mermaid";
    window.mermaid = {
        initialize: function () {},
        parse: function () { return Promise.reject(new Error(MSG)); },
        render: function () { return Promise.reject(new Error(MSG)); }
    };
})();
"#;
        match fs::write(dest, stub) {
            Ok(()) => {
                println!(
                    "cargo:warning=mermaid bundle missing (node_modules/mermaid not found). Using a stub — diagrams will show an error until you run `npm ci && npm run sync:mermaid`."
                );
            }
            Err(e) => {
                println!(
                    "cargo:warning=failed to write mermaid stub to assets/mermaid.min.js: {e}"
                );
            }
        }
    }
}
