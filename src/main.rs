mod db;
mod events;
mod ingestor;
mod nntp;
mod patch;
mod settings;

use db::Database;
use events::Event;
use ingestor::Ingestor;
use settings::Settings;
use tokio::sync::mpsc;
use tracing::{error, info};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Initialize tracing with EnvFilter
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();

    info!("Starting Sashiko...");

    // Load settings
    let settings = match Settings::new() {
        Ok(s) => {
            info!("Settings loaded successfully");
            s
        }
        Err(e) => {
            error!("Failed to load settings: {}", e);
            return Err(e.into());
        }
    };

    info!("Settings: {:?}", settings);

    // Initialize Database
    let db = Database::new(settings.database).await?;
    db.migrate().await?;

    // Create internal task queue
    let (tx, mut rx) = mpsc::channel::<Event>(100);

    // Spawn Worker (Placeholder)
    tokio::spawn(async move {
        info!("Worker started");
        while let Some(event) = rx.recv().await {
            info!("Worker received event: {:?}", event);
        }
    });

    // Start Ingestor
    let ingestor = Ingestor::new(settings.nntp, db, tx);
    tokio::spawn(async move {
        if let Err(e) = ingestor.run().await {
            error!("Ingestor fatal error: {}", e);
        }
    });

    // Keep the main thread running
    tokio::signal::ctrl_c().await?;
    info!("Shutting down...");

    Ok(())
}
