use crate::compression::compress_string_if_needed;
use crate::db::Database;
use anyhow::Result;
use libsql::Value;
use std::sync::Arc;
use tokio::time::{Duration, sleep};
use tracing::{error, info};

pub async fn run_compressor(db: Arc<Database>) {
    info!("Starting background data compressor");
    loop {
        let batch_size = 50;
        let mut work_done = false;

        match compress_messages(&db, batch_size).await {
            Ok(count) => {
                if count > 0 {
                    work_done = true;
                }
            }
            Err(e) => error!("Error compressing messages: {:?}", e),
        }

        match compress_patches(&db, batch_size).await {
            Ok(count) => {
                if count > 0 {
                    work_done = true;
                }
            }
            Err(e) => error!("Error compressing patches: {:?}", e),
        }

        match compress_patchsets(&db, batch_size).await {
            Ok(count) => {
                if count > 0 {
                    work_done = true;
                }
            }
            Err(e) => error!("Error compressing patchsets: {:?}", e),
        }

        match compress_reviews(&db, batch_size).await {
            Ok(count) => {
                if count > 0 {
                    work_done = true;
                }
            }
            Err(e) => error!("Error compressing reviews: {:?}", e),
        }

        match compress_ai_interactions(&db, batch_size).await {
            Ok(count) => {
                if count > 0 {
                    work_done = true;
                }
            }
            Err(e) => error!("Error compressing ai_interactions: {:?}", e),
        }

        if !work_done {
            info!("Compression sweep complete. Sleeping.");
            sleep(Duration::from_secs(3600)).await;
        } else {
            sleep(Duration::from_secs(5)).await;
        }
    }
}

async fn compress_messages(db: &Database, limit: i32) -> Result<usize> {
    let mut rows = db.conn.query(
        "SELECT id, body FROM messages WHERE typeof(body) = 'text' AND length(body) > 1024 LIMIT ?",
        libsql::params![limit],
    ).await?;

    let mut to_update = Vec::new();
    while let Ok(Some(row)) = rows.next().await {
        let id: i64 = row.get(0)?;
        let body: String = row.get(1)?;
        to_update.push((id, body));
    }

    let count = to_update.len();
    if count == 0 {
        return Ok(0);
    }

    let compressed: Vec<(i64, Value)> = tokio::task::spawn_blocking(move || {
        to_update
            .into_iter()
            .map(|(id, body)| (id, compress_string_if_needed(&body)))
            .collect()
    })
    .await?;

    db.begin_transaction().await?;
    let mut success = true;
    for (id, val) in compressed {
        if let Err(e) = db
            .conn
            .execute(
                "UPDATE messages SET body = ? WHERE id = ?",
                libsql::params![val, id],
            )
            .await
        {
            error!("Fail in messages update: {}", e);
            success = false;
            break;
        }
    }
    if success {
        db.commit_transaction().await?;
    } else {
        let _ = db.conn.execute("ROLLBACK", ()).await;
    }
    Ok(count)
}

async fn compress_patches(db: &Database, limit: i32) -> Result<usize> {
    let mut rows = db.conn.query(
        "SELECT id, diff FROM patches WHERE typeof(diff) = 'text' AND length(diff) > 1024 LIMIT ?",
        libsql::params![limit],
    ).await?;

    let mut to_update = Vec::new();
    while let Ok(Some(row)) = rows.next().await {
        let id: i64 = row.get(0)?;
        let diff: String = row.get(1)?;
        to_update.push((id, diff));
    }

    let count = to_update.len();
    if count == 0 {
        return Ok(0);
    }

    let compressed: Vec<(i64, Value)> = tokio::task::spawn_blocking(move || {
        to_update
            .into_iter()
            .map(|(id, diff)| (id, compress_string_if_needed(&diff)))
            .collect()
    })
    .await?;

    db.begin_transaction().await?;
    let mut success = true;
    for (id, val) in compressed {
        if let Err(e) = db
            .conn
            .execute(
                "UPDATE patches SET diff = ? WHERE id = ?",
                libsql::params![val, id],
            )
            .await
        {
            error!("Fail in patches update: {}", e);
            success = false;
            break;
        }
    }
    if success {
        db.commit_transaction().await?;
    } else {
        let _ = db.conn.execute("ROLLBACK", ()).await;
    }
    Ok(count)
}

async fn compress_patchsets(db: &Database, limit: i32) -> Result<usize> {
    let mut rows = db.conn.query(
        "SELECT id, baseline_logs FROM patchsets WHERE typeof(baseline_logs) = 'text' AND length(baseline_logs) > 1024 LIMIT ?",
        libsql::params![limit],
    ).await?;

    let mut to_update = Vec::new();
    while let Ok(Some(row)) = rows.next().await {
        let id: i64 = row.get(0)?;
        let logs: String = row.get(1)?;
        to_update.push((id, logs));
    }

    let count = to_update.len();
    if count == 0 {
        return Ok(0);
    }

    let compressed: Vec<(i64, Value)> = tokio::task::spawn_blocking(move || {
        to_update
            .into_iter()
            .map(|(id, logs)| (id, compress_string_if_needed(&logs)))
            .collect()
    })
    .await?;

    db.begin_transaction().await?;
    let mut success = true;
    for (id, val) in compressed {
        if let Err(e) = db
            .conn
            .execute(
                "UPDATE patchsets SET baseline_logs = ? WHERE id = ?",
                libsql::params![val, id],
            )
            .await
        {
            error!("Fail in patchsets update: {}", e);
            success = false;
            break;
        }
    }
    if success {
        db.commit_transaction().await?;
    } else {
        let _ = db.conn.execute("ROLLBACK", ()).await;
    }
    Ok(count)
}

async fn compress_reviews(db: &Database, limit: i32) -> Result<usize> {
    let mut count = 0;

    // Logs
    let mut rows = db.conn.query(
        "SELECT id, logs FROM reviews WHERE typeof(logs) = 'text' AND length(logs) > 1024 LIMIT ?",
        libsql::params![limit],
    ).await?;

    let mut to_update_logs = Vec::new();
    while let Ok(Some(row)) = rows.next().await {
        let id: i64 = row.get(0)?;
        let logs: String = row.get(1)?;
        to_update_logs.push((id, logs));
    }

    if !to_update_logs.is_empty() {
        count += to_update_logs.len();
        let compressed: Vec<(i64, Value)> = tokio::task::spawn_blocking(move || {
            to_update_logs
                .into_iter()
                .map(|(id, l)| (id, compress_string_if_needed(&l)))
                .collect()
        })
        .await?;

        db.begin_transaction().await?;
        let mut success = true;
        for (id, val) in compressed {
            if let Err(e) = db
                .conn
                .execute(
                    "UPDATE reviews SET logs = ? WHERE id = ?",
                    libsql::params![val, id],
                )
                .await
            {
                error!("Fail in reviews log update: {}", e);
                success = false;
                break;
            }
        }
        if success {
            db.commit_transaction().await?;
        } else {
            let _ = db.conn.execute("ROLLBACK", ()).await;
        }
    }

    // Inline review
    let mut rows = db.conn.query(
        "SELECT id, inline_review FROM reviews WHERE typeof(inline_review) = 'text' AND length(inline_review) > 1024 LIMIT ?",
        libsql::params![limit],
    ).await?;

    let mut to_update_inline = Vec::new();
    while let Ok(Some(row)) = rows.next().await {
        let id: i64 = row.get(0)?;
        let inline: String = row.get(1)?;
        to_update_inline.push((id, inline));
    }

    if !to_update_inline.is_empty() {
        count += to_update_inline.len();
        let compressed: Vec<(i64, Value)> = tokio::task::spawn_blocking(move || {
            to_update_inline
                .into_iter()
                .map(|(id, i)| (id, compress_string_if_needed(&i)))
                .collect()
        })
        .await?;

        db.begin_transaction().await?;
        let mut success = true;
        for (id, val) in compressed {
            if let Err(e) = db
                .conn
                .execute(
                    "UPDATE reviews SET inline_review = ? WHERE id = ?",
                    libsql::params![val, id],
                )
                .await
            {
                error!("Fail in reviews inline update: {}", e);
                success = false;
                break;
            }
        }
        if success {
            db.commit_transaction().await?;
        } else {
            let _ = db.conn.execute("ROLLBACK", ()).await;
        }
    }

    Ok(count)
}

async fn compress_ai_interactions(db: &Database, limit: i32) -> Result<usize> {
    let mut rows = db.conn.query(
        "SELECT id, input_context, output_raw FROM ai_interactions WHERE (typeof(input_context) = 'text' AND length(input_context) > 1024) OR (typeof(output_raw) = 'text' AND length(output_raw) > 1024) LIMIT ?",
        libsql::params![limit],
    ).await?;

    let mut to_update = Vec::new();
    while let Ok(Some(row)) = rows.next().await {
        let id: String = row.get(0)?;
        let input_val: Value = row.get(1)?;
        let output_val: Value = row.get(2)?;
        to_update.push((id, input_val, output_val));
    }

    let count = to_update.len();
    if count == 0 {
        return Ok(0);
    }

    let compressed: Vec<(String, Value, Value)> = tokio::task::spawn_blocking(move || {
        to_update
            .into_iter()
            .map(|(id, input_val, output_val)| {
                let new_input = match input_val {
                    Value::Text(s) => compress_string_if_needed(&s),
                    other => other,
                };
                let new_output = match output_val {
                    Value::Text(s) => compress_string_if_needed(&s),
                    other => other,
                };
                (id, new_input, new_output)
            })
            .collect()
    })
    .await?;

    db.begin_transaction().await?;
    let mut success = true;
    for (id, new_input, new_output) in compressed {
        if let Err(e) = db
            .conn
            .execute(
                "UPDATE ai_interactions SET input_context = ?, output_raw = ? WHERE id = ?",
                libsql::params![new_input, new_output, id],
            )
            .await
        {
            error!("Fail in ai_interactions update: {}", e);
            success = false;
            break;
        }
    }
    if success {
        db.commit_transaction().await?;
    } else {
        let _ = db.conn.execute("ROLLBACK", ()).await;
    }

    Ok(count)
}
