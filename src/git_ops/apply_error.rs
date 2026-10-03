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

/// Best-effort, informational classification of an unsuccessful patch application.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PatchApplyFailure {
    /// A file required by the patch is absent from the index.
    MissingFile,
    /// A file the patch adds is already in the index.
    ExistingFile,
    /// Git could not match the patch's context.
    ContextMismatch,
    /// Git could not parse the patch or recognize its format.
    MalformedPatch,
    /// Git stopped because the patch contains no changes.
    EmptyPatch,
    /// The diagnostic is unavailable, unrecognized, or ambiguous.
    Unknown,
}

impl PatchApplyFailure {
    /// Classifies only typed Git application failures, including wrapped errors.
    pub fn from_error(error: &anyhow::Error) -> Self {
        error
            .downcast_ref::<GitAmError>()
            .map_or(Self::Unknown, |error| error.kind)
    }

    /// Returns a fixed public explanation without any process output or patch data.
    pub fn public_message(self) -> &'static str {
        match self {
            Self::MissingFile => "A required file is missing from the index.",
            Self::ExistingFile => "A file to be added already exists in the index.",
            Self::ContextMismatch => "Patch context does not match.",
            Self::MalformedPatch => "Malformed or unsupported patch format.",
            Self::EmptyPatch => "Patch contains no changes.",
            Self::Unknown => "Unrecognized patch application failure.",
        }
    }

    fn classify(stdout: &str, stderr: &str) -> Self {
        // Git emits this standalone marker on stdout before echoing a subject.
        let mut kind = (stdout == "Patch is empty.").then_some(Self::EmptyPatch);
        for line in stderr.lines() {
            if let Some(detected) = Self::from_diagnostic(line) {
                if kind.is_some_and(|previous| previous != detected) {
                    return Self::Unknown;
                }
                kind = Some(detected);
            } else if line.starts_with("error: ") || line.starts_with("fatal: ") {
                return Self::Unknown;
            }
        }
        kind.unwrap_or(Self::Unknown)
    }

    fn from_diagnostic(line: &str) -> Option<Self> {
        if matches!(
            line,
            "Patch format detection failed."
                | "error: No valid patches in input (allow with \"--allow-empty\")"
        ) {
            return Some(Self::MalformedPatch);
        }
        let message = line.strip_prefix("error: ")?;
        for (suffix, kind) in [
            (": does not exist in index", Self::MissingFile),
            (": already exists in index", Self::ExistingFile),
            (": patch does not apply", Self::ContextMismatch),
        ] {
            if message
                .strip_suffix(suffix)
                .is_some_and(|path| !path.is_empty())
            {
                return Some(kind);
            }
        }
        if message.starts_with("patch failed: ") {
            return Some(Self::ContextMismatch);
        }
        // Git uses either "line N" or "path:N", depending on its version.
        if message
            .strip_prefix("corrupt patch at ")
            .and_then(|location| {
                location.strip_prefix("line ").or_else(|| {
                    location
                        .rsplit_once(':')
                        .filter(|(path, _)| !path.is_empty())
                        .map(|(_, line)| line)
                })
            })
            .is_some_and(|line| line.parse::<u64>().is_ok())
            || message.starts_with("git diff header lacks filename information ")
            || message.starts_with("patch fragment without header at ")
        {
            return Some(Self::MalformedPatch);
        }
        None
    }
}

// Preserve the detailed error display for existing callers. Public baseline logs
// must use PatchApplyFailure::public_message instead of formatting this error.
#[derive(Debug, thiserror::Error)]
#[error("git am failed. stdout: {stdout}\nstderr: {stderr}")]
pub(super) struct GitAmError {
    kind: PatchApplyFailure,
    stdout: String,
    stderr: String,
}

impl GitAmError {
    pub(super) fn new(stdout: &[u8], stderr: &[u8]) -> Self {
        let stdout = String::from_utf8_lossy(stdout);
        let stderr = String::from_utf8_lossy(stderr);
        Self {
            kind: PatchApplyFailure::classify(stdout.trim_end_matches(['\r', '\n']), &stderr),
            stdout: stdout.trim().to_string(),
            stderr: stderr.trim().to_string(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_known_diagnostics() {
        for (stderr, expected) in [
            (
                "error: file.c: does not exist in index",
                PatchApplyFailure::MissingFile,
            ),
            (
                "error: file.c: already exists in index",
                PatchApplyFailure::ExistingFile,
            ),
            (
                "error: patch failed: file.c:42\nerror: file.c: patch does not apply\nhint: retry",
                PatchApplyFailure::ContextMismatch,
            ),
            (
                "Patch format detection failed.",
                PatchApplyFailure::MalformedPatch,
            ),
            (
                "error: corrupt patch at line 12",
                PatchApplyFailure::MalformedPatch,
            ),
            (
                "error: corrupt patch at /tmp/private:work tree/patch:12",
                PatchApplyFailure::MalformedPatch,
            ),
            (
                "error: git diff header lacks filename information (line 5)",
                PatchApplyFailure::MalformedPatch,
            ),
            (
                "error: git diff header lacks filename information at /tmp/patch:5",
                PatchApplyFailure::MalformedPatch,
            ),
            (
                "error: patch fragment without header at line 5: @@ -1 +1 @@",
                PatchApplyFailure::MalformedPatch,
            ),
            (
                "error: patch fragment without header at /tmp/private:work tree/patch:5: @@ -1 +1 @@",
                PatchApplyFailure::MalformedPatch,
            ),
            (
                "error: No valid patches in input (allow with \"--allow-empty\")",
                PatchApplyFailure::MalformedPatch,
            ),
            (
                "warning: whitespace\nerror: a: does not exist in index\nerror: b: does not exist in index",
                PatchApplyFailure::MissingFile,
            ),
        ] {
            assert_eq!(
                PatchApplyFailure::classify("Applying: subject", stderr),
                expected,
                "{stderr}"
            );
        }
    }

    #[test]
    fn unknown_or_conflicting_diagnostics_stay_unknown() {
        for stderr in [
            "",
            "warning: file.c: does not exist in index",
            "  error: file.c: does not exist in index",
            "error: : does not exist in index",
            "error: file.c: does not exist in index extra text",
            "error: corrupt patch at line not-a-number",
            "error: corrupt patch at :12",
            "error: corrupt patch at /tmp/patch:",
            "error: corrupt patch at /tmp/patch:not-a-number",
            "error: corrupt patch at /tmp/patch:12 extra text",
            "fatal: unknown repository failure",
            "Fehler: Datei fehlt",
            "error: a: does not exist in index\nerror: b: already exists in index",
            "error: a: does not exist in index\nfatal: unknown repository failure",
            "error: unknown failure\nerror: a: does not exist in index",
        ] {
            assert_eq!(
                PatchApplyFailure::classify("", stderr),
                PatchApplyFailure::Unknown,
                "{stderr}"
            );
        }
    }

    #[test]
    fn only_the_standalone_empty_patch_marker_is_recognized_on_stdout() {
        assert_eq!(
            PatchApplyFailure::classify("Patch is empty.", "hint: resolve the failure"),
            PatchApplyFailure::EmptyPatch
        );
        for stdout in [
            "Applying: Patch is empty.",
            "Applying: error: file.c: does not exist in index",
            "Applying: subject\nPatch is empty.",
            "No changes -- Patch already applied.",
        ] {
            assert_eq!(
                PatchApplyFailure::classify(stdout, ""),
                PatchApplyFailure::Unknown
            );
        }
        assert_eq!(
            PatchApplyFailure::classify("Patch is empty.", "fatal: unknown failure"),
            PatchApplyFailure::Unknown
        );
        assert_eq!(
            PatchApplyFailure::classify(
                "Patch is empty.",
                "error: file.c: does not exist in index"
            ),
            PatchApplyFailure::Unknown
        );
    }

    #[test]
    fn malformed_patch_locations_are_not_published() {
        for stderr in [
            "error: corrupt patch at /tmp/private-token=secret/patch:12",
            "error: patch fragment without header at /tmp/private-token=secret/patch:5: @@ -1 +1 @@",
        ] {
            let error = anyhow::Error::new(GitAmError::new(b"", stderr.as_bytes()));
            assert_eq!(
                PatchApplyFailure::from_error(&error).public_message(),
                "Malformed or unsupported patch format."
            );
        }
    }

    #[test]
    fn public_messages_do_not_include_process_output_or_error_context() {
        let error = anyhow::Error::new(GitAmError::new(
            b"Applying: private subject https://user:password@example.com",
            b"error: <script>private-file</script>: does not exist in index",
        ))
        .context("private error context");
        assert_eq!(
            PatchApplyFailure::from_error(&error),
            PatchApplyFailure::MissingFile
        );
        assert_eq!(
            PatchApplyFailure::from_error(&error).public_message(),
            "A required file is missing from the index."
        );
        // Other callers retain the pre-existing detailed representation.
        let details = format!("{error:#}");
        assert!(details.contains("git am failed. stdout: Applying: private subject"));
        assert!(details.contains("stderr: error: <script>private-file</script>"));

        let unrelated = anyhow::anyhow!("error: private-file: does not exist in index");
        assert_eq!(
            PatchApplyFailure::from_error(&unrelated),
            PatchApplyFailure::Unknown
        );
        assert_eq!(
            PatchApplyFailure::from_error(&unrelated).public_message(),
            "Unrecognized patch application failure."
        );
        let indented = GitAmError::new(b"", b"  error: private-file: does not exist in index");
        assert_eq!(indented.kind, PatchApplyFailure::Unknown);
    }
}
