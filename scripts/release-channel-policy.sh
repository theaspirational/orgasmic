#!/usr/bin/env bash
# orgasmic:dec_B4147
# Shared policy for the four rolling GitHub release channels. Source this file;
# callers choose whether they need only release state or full display metadata.
# shellcheck disable=SC2034 # Output globals are consumed by the sourcing script.

release_channel_state_policy() {
    local line="$1"
    local channel="$2"

    case "$line:$channel" in
        runtime:stable)
            RELEASE_POLICY_LATEST="true"
            RELEASE_POLICY_PRERELEASE="false"
            ;;
        runtime:nightly)
            RELEASE_POLICY_LATEST="false"
            RELEASE_POLICY_PRERELEASE="true"
            ;;
        apps:stable)
            RELEASE_POLICY_LATEST="false"
            RELEASE_POLICY_PRERELEASE="false"
            ;;
        apps:nightly)
            RELEASE_POLICY_LATEST="false"
            RELEASE_POLICY_PRERELEASE="true"
            ;;
        *)
            echo "error: unsupported release policy tuple: $line/$channel" >&2
            return 1
            ;;
    esac
}

release_channel_metadata_policy() {
    local line="$1"
    local channel="$2"
    local version="$3"
    local commit="$4"

    release_channel_state_policy "$line" "$channel" || return 1
    case "$line:$channel" in
        runtime:stable)
            RELEASE_POLICY_TITLE="Orgasmic Runtime $version"
            RELEASE_POLICY_NOTES="Runtime bundles $version from $commit."
            ;;
        runtime:nightly)
            RELEASE_POLICY_TITLE="Orgasmic Runtime Nightly"
            RELEASE_POLICY_NOTES="Runtime bundles $version from $commit."
            ;;
        apps:stable)
            RELEASE_POLICY_TITLE="Orgasmic Apps $version"
            RELEASE_POLICY_NOTES="App builds $version from $commit."
            ;;
        apps:nightly)
            RELEASE_POLICY_TITLE="Orgasmic Apps Nightly"
            RELEASE_POLICY_NOTES="App builds $version from $commit."
            ;;
    esac
}
