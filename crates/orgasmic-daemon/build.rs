use std::fs;
use std::path::PathBuf;
use std::process::Command;

fn main() {
    let manifest_dir = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    let repo_root = manifest_dir.join("../..");
    let ui_dist = repo_root.join("ui/dist");

    println!(
        "cargo:rerun-if-changed={}",
        repo_root.join("ui/src").display()
    );
    println!(
        "cargo:rerun-if-changed={}",
        repo_root.join("ui/index.html").display()
    );
    println!(
        "cargo:rerun-if-changed={}",
        repo_root.join("ui/vite.config.ts").display()
    );
    println!(
        "cargo:rerun-if-changed={}",
        repo_root.join("ui/package.json").display()
    );
    println!(
        "cargo:rerun-if-changed={}",
        repo_root.join("ui/package-lock.json").display()
    );
    println!("cargo:rerun-if-changed={}", ui_dist.display());

    // Skip the npm build in debug/test builds. Only embed the real UI for
    // release builds or when ORGASMIC_EMBED_UI=1 is set explicitly.
    // This lets `cargo test` and `cargo build` work without node_modules.
    let profile = std::env::var("PROFILE").unwrap_or_default();
    let embed_ui = std::env::var("ORGASMIC_EMBED_UI").as_deref() == Ok("1");

    // orgasmic:dec_B4147 — when a caller has already built ui/dist once and sets
    // ORGASMIC_UI_PREBUILT=1, reuse it instead of re-running npm per target. The
    // local publish pipeline (scripts/publish-runtime.sh) builds the UI once and
    // then compiles all four runtime targets, so the npm build must not fire four
    // times. Guarded by the existence check so a stale opt-in degrades to a fresh
    // build rather than embedding a missing dist.
    let reuse_prebuilt = std::env::var("ORGASMIC_UI_PREBUILT").as_deref() == Ok("1")
        && ui_dist.join("index.html").exists();

    let dist_dir = if profile == "release" || embed_ui {
        if reuse_prebuilt {
            println!(
                "cargo:warning=reusing prebuilt UI at {} (ORGASMIC_UI_PREBUILT=1)",
                ui_dist.display()
            );
        } else {
            run_npm_build(&repo_root);
        }
        assert!(
            ui_dist.join("index.html").exists(),
            "npm build did not produce {}/index.html — check ui/dist after `npm --prefix ui run build`",
            ui_dist.display()
        );
        ui_dist
    } else {
        // Non-release: write a placeholder dist so include_dir! compiles cleanly
        // without requiring npm or node_modules. The embedded UI is a stub.
        let out_dir = PathBuf::from(std::env::var("OUT_DIR").unwrap());
        let placeholder = out_dir.join("ui-placeholder-dist");
        fs::create_dir_all(&placeholder).unwrap();
        fs::write(
            placeholder.join("index.html"),
            b"<!-- placeholder UI: rebuild with `cargo build --release` or ORGASMIC_EMBED_UI=1 -->\n",
        )
        .unwrap();
        placeholder
    };

    println!(
        "cargo:rustc-env=ORGASMIC_UI_DIST_DIR={}",
        dist_dir.display()
    );
}

fn run_npm_build(repo_root: &std::path::Path) {
    // On Windows `npm` is a `.cmd` shim that `CreateProcess` won't resolve from a
    // bare `npm`, so launch it through `cmd /C` (PATHEXT finds the shim). Other
    // platforms run npm directly.
    let result = if cfg!(windows) {
        Command::new("cmd")
            .args(["/C", "npm", "--prefix", "ui", "run", "build"])
            .current_dir(repo_root)
            .status()
    } else {
        Command::new("npm")
            .args(["--prefix", "ui", "run", "build"])
            .current_dir(repo_root)
            .status()
    };

    match result {
        Err(e) => panic!(
            "UI build required but npm could not be launched: {e}\n\
             Fix: install Node.js and npm, then run `npm --prefix ui install`.\n\
             To skip during development, use a debug build (not --release) \
             and do not set ORGASMIC_EMBED_UI=1."
        ),
        Ok(status) if !status.success() => panic!(
            "`npm --prefix ui run build` failed (exit {code}).\n\
             Fix: run `cd ui && npm install` if node_modules is missing, \
             or check TypeScript errors with `npm --prefix ui run typecheck`.\n\
             To skip during development, use a debug build (not --release) \
             and do not set ORGASMIC_EMBED_UI=1.",
            code = status
                .code()
                .map(|c| c.to_string())
                .unwrap_or_else(|| "?".into())
        ),
        Ok(_) => {}
    }
}
