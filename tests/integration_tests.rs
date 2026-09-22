mod integration {
    mod cli_tests;
    #[cfg(feature = "server")]
    mod cover_letter_late_merge_test;
    #[cfg(feature = "server")]
    mod db_version_merge_test;
    #[cfg(feature = "server")]
    mod findings_sum_test;
    #[cfg(feature = "server")]
    mod merge_bug_different_series;
    #[cfg(feature = "server")]
    mod merge_bug_loose_patch_count;
    #[cfg(feature = "server")]
    mod merge_bug_prefixes;
    #[cfg(feature = "server")]
    mod merge_should_happen;
    mod prefetch_e2e_test;
    #[cfg(feature = "server")]
    mod quilt_merge_test;
    #[cfg(feature = "server")]
    mod server_tests;
    #[cfg(feature = "server")]
    mod singleton_cover_merge_test;
    #[cfg(feature = "server")]
    mod singleton_root_overwrite;
    #[cfg(feature = "server")]
    mod test_nested;
    #[cfg(feature = "server")]
    mod test_pending;
}
