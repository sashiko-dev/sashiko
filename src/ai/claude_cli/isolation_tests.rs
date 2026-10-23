// Copyright 2026 The Sashiko Authors
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     https://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use super::*;

#[test]
fn cache_identity_excludes_unrestricted_responses() {
    let model = "claude-sonnet-4-6";
    for effort in [None, Some("high")] {
        let legacy = cache_identity_with(model, &[("effort", effort)]);
        let provider = ClaudeCliProvider {
            model: model.to_string(),
            effort: effort.map(str::to_string),
        };
        assert_ne!(provider.cache_identity(), legacy);
        let before_json_schema = cache_identity_with(
            model,
            &[
                ("effort", effort),
                ("completion_profile", Some("restricted-v1")),
            ],
        );
        assert_ne!(provider.cache_identity(), before_json_schema);
    }
}

#[test]
fn cache_identity_tracks_model_and_effort() {
    let identity = |model: &str, effort: Option<&str>| {
        ClaudeCliProvider {
            model: model.to_string(),
            effort: effort.map(str::to_string),
        }
        .cache_identity()
    };
    let plain = identity("claude-sonnet-4-6", None);
    assert_ne!(plain, identity("claude-sonnet-4-6", Some("high")));
    assert_ne!(plain, identity("claude-opus-4-7", None));
}

#[test]
fn working_directory_errors_classify_by_cause() {
    use std::io::ErrorKind;

    for kind in [
        ErrorKind::PermissionDenied,
        ErrorKind::NotFound,
        ErrorKind::InvalidInput,
        ErrorKind::InvalidFilename,
        ErrorKind::NotADirectory,
        ErrorKind::ReadOnlyFilesystem,
        ErrorKind::Unsupported,
    ] {
        let error = ClaudeCliError::WorkingDirectory(kind.into());
        assert!(matches!(error.ai_error_class(), AiErrorClass::Fatal));
    }
    for kind in [
        ErrorKind::WouldBlock,
        ErrorKind::Interrupted,
        ErrorKind::StorageFull,
        ErrorKind::ResourceBusy,
        ErrorKind::OutOfMemory,
        ErrorKind::Other,
    ] {
        let error = ClaudeCliError::WorkingDirectory(kind.into());
        assert!(matches!(
            error.ai_error_class(),
            AiErrorClass::Transient { retry_after }
                if retry_after == Duration::from_secs(30)
        ));
    }
}
