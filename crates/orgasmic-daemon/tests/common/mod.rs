//! Shared integration-test helpers (path-free error assertions).

#![allow(dead_code)]

use std::path::Path;

pub fn assert_body_rejects_paths(body: &str, reject_paths: &[&Path]) {
    for path in reject_paths {
        let display = path.display().to_string();
        assert!(
            !body.contains(&display),
            "body leaked fixture path {display}: {body}"
        );
        if let Ok(canonical) = std::fs::canonicalize(path) {
            let canonical = canonical.display().to_string();
            assert!(
                !body.contains(&canonical),
                "body leaked canonical fixture path {canonical}: {body}"
            );
        }
    }
    assert!(!body.contains("/tmp"), "body leaked /tmp path: {body}");
    assert!(
        !body.contains("/private/"),
        "body leaked /private path: {body}"
    );
    assert!(
        !body.contains("os error"),
        "body leaked OS errno detail: {body}"
    );
}

pub fn assert_path_free_error(
    body: &str,
    expected_fragment: &str,
    reject_paths: &[&Path],
) -> String {
    let json: serde_json::Value = serde_json::from_str(body)
        .unwrap_or_else(|e| panic!("error body is not JSON: {e}: {body}"));
    let error = json
        .get("error")
        .and_then(|value| value.as_str())
        .unwrap_or_else(|| panic!("error body missing string error field: {body}"));
    assert!(!error.is_empty(), "error field is empty: {body}");
    assert!(
        error.contains(expected_fragment),
        "expected error field to contain {expected_fragment:?}, got {error:?}"
    );
    assert_body_rejects_paths(body, reject_paths);
    error.to_string()
}

pub async fn assert_path_free_error_response(
    resp: reqwest::Response,
    expected_status: reqwest::StatusCode,
    expected_fragment: &str,
    reject_paths: &[&Path],
) -> String {
    assert_eq!(resp.status(), expected_status);
    let body = resp.text().await.unwrap();
    assert_path_free_error(&body, expected_fragment, reject_paths)
}
