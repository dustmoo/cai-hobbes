use std::fs;
use std::path::Path;

fn main() {
    // This crate compiles the shared src/ but not the root build.rs, so it
    // must stage the gitignored assets/mermaid.min.js itself (include_str! in
    // src/components/mermaid.rs). Same degrade-to-stub rule as the root.
    sync_mermaid_bundle();

    #[cfg(target_os = "windows")]
    {
        let mut res = winres::WindowsResource::new();
        res.set_icon("../../assets/icon.ico");
        res.compile().expect("Failed to compile Windows resources");
    }
}

fn sync_mermaid_bundle() {
    let src = Path::new("../../node_modules/mermaid/dist/mermaid.min.js");
    let dest = Path::new("../../assets/mermaid.min.js");
    println!("cargo:rerun-if-changed={}", src.display());
    if src.exists() {
        if let Err(e) = fs::copy(src, dest) {
            println!("cargo:warning=failed to copy mermaid.min.js from node_modules ({e})");
        }
    }
    if !dest.exists() {
        let stub = r#"(function () {
    var MSG = "mermaid bundle missing \u2014 run npm ci && npm run sync:mermaid";
    window.mermaid = {
        initialize: function () {},
        parse: function () { return Promise.reject(new Error(MSG)); },
        render: function () { return Promise.reject(new Error(MSG)); }
    };
})();
"#;
        if let Err(e) = fs::write(dest, stub) {
            println!("cargo:warning=failed to write mermaid stub: {e}");
        } else {
            println!("cargo:warning=mermaid bundle missing; using a stub");
        }
    }
}
