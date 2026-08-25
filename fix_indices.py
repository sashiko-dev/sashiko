with open('src/db.rs', 'r') as f:
    text = f.read()

start_idx = text.find("    fn parse_bug_row")
end_idx = text.find("        Ok(Bug", start_idx)

new_block = """    fn parse_bug_row(row: &libsql::Row) -> Result<Bug> {
        let id: i64 = row.get(0)?;
        let slug: String = row.get(1)?;
        let status: String = row.get(2)?;
        let problem: String = row.get(3)?;
        let severity_val: i32 = row.get(4)?;
        let severity_explanation: Option<String> = row.get(5).ok().flatten();
        let locations_str: Option<String> = row.get(6).ok().flatten();
        let locations: Option<serde_json::Value> =
            locations_str.and_then(|s| serde_json::from_str(&s).ok());
        let subsystems_str: Option<String> = row.get(7).ok().flatten();
        let subsystems: Vec<String> = match subsystems_str {
            Some(ref s) => serde_json::from_str(s).unwrap_or_else(|_| {
                if !s.trim().is_empty() {
                    vec![s.trim().to_string()]
                } else {
                    Vec::new()
                }
            }),
            None => Vec::new(),
        };
        let source_files_str: Option<String> = row.get(8).ok().flatten();
        let source_files: Option<Vec<String>> =
            source_files_str.and_then(|s| serde_json::from_str(&s).ok());
        let inline_review: String = crate::compression::get_compressed_string_opt(row, 9)
            .unwrap_or(None)
            .unwrap_or_else(|| row.get::<String>(9).unwrap_or_default());
        let logs: Option<String> = crate::compression::get_compressed_string_opt(row, 10)
            .unwrap_or(None)
            .or_else(|| row.get::<Option<String>>(10).ok().flatten());
        let vector_json: Option<String> = row.get(11).ok().flatten();
        let discovered_in_patchset_id: Option<i64> = row.get(12).ok().flatten();
        let discovered_in_patch_id: Option<i64> = row.get(13).ok().flatten();
        let discovered_in_commit: Option<String> = row.get(14).ok().flatten();
        let introduced_in_commit: Option<String> = row.get(15).ok().flatten();
        let is_fixed: bool = row.get::<i64>(16).unwrap_or(0) != 0;
        let fixed_in_commit: Option<String> = row.get(17).ok().flatten();
        let created_at: i64 = row.get(18)?;

"""

text = text[:start_idx] + new_block + text[end_idx:]

with open('src/db.rs', 'w') as f:
    f.write(text)
