fn main() {
    // Declare our own (#[tauri::command]) commands so tauri-build autogenerates
    // `allow-*` permissions for them under the app ACL. This is required because
    // the full UI is served by the daemon on a *remote* origin (127.0.0.1:4848),
    // and Tauri v2 refuses to expose app commands to any non-local origin unless
    // a capability explicitly grants them. Declaring an app manifest also turns
    // on ACL enforcement for app commands on the local origin, so every command
    // the frontend invokes must be listed here AND granted in capabilities. See
    // capabilities/default.json. dec_PVDP3.
    tauri_build::try_build(
        tauri_build::Attributes::new().app_manifest(
            tauri_build::AppManifest::new().commands(&[
                "runtime_probe",
                "runtime_launch_url",
                "local_backend_profile",
                "check_app_update",
                "install_app_update",
                "update_runtime",
                "check_android_update",
            ]),
        ),
    )
    .expect("failed to run tauri-build");
}
