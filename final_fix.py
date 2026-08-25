import re

with open('src/pipelines/bug.rs', 'r') as f:
    text = f.read()

# 1. Filter out the current bug instance from known_bugs
bad_known = r'let known_bugs = db\.list_all_bugs_for_vector_search\(\)\.await\?;'
good_known = r'''let mut known_bugs = db.list_all_bugs_for_vector_search().await?;
    known_bugs.retain(|b| b.id != bug_row.id);'''
text = re.sub(bad_known, good_known, text)

# 2. Fix test_process_issue_dedup_first_short_circuits_verification body
start_dedup = text.find('async fn test_process_issue_dedup_first_short_circuits_verification() {')
start_outcome = text.find('let outcome = process_issue', start_dedup)
end_test = text.find('}\n}', start_dedup)

new_block = '''let outcome = process_issue(&provider, None, &db, input.clone(), None)
            .await
            .unwrap();

        let final_outcome = match outcome {
            BugOutcome::NewlyDiscovered { ref bug } => process_issue_worker(&provider, None, &db, &bug, input.clone(), None).await.unwrap(),
            _ => panic!("Expected NewlyDiscovered initially"),
        };

        match final_outcome {
            BugOutcome::Duplicate {
                existing_bug,
                reasoning,
                logs,
            } => {
                assert_eq!(existing_bug.id, existing_id);
                assert_eq!(existing_bug.slug, "pb-existing1");
                assert_eq!(reasoning, "Exact match with known bug #1 in e1000 driver");
                assert!(logs.is_some());
            }
            _ => panic!("Expected Duplicate outcome, got {:?}", final_outcome),
        }
    '''

text = text[:start_outcome] + new_block + text[end_test:]

with open('src/pipelines/bug.rs', 'w') as f:
    f.write(text)

