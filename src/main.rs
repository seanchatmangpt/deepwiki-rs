use crate::generator::workflow::launch;
use anyhow::Result;
use clap::Parser;

mod cache;
mod cli;
mod config;
mod generator;
mod i18n;
mod integrations;
mod llm;
mod memory;
mod semantic;
mod types;
mod utils;

#[tokio::main]
async fn main() -> Result<()> {
    let args = cli::Args::parse();

    // Handle subcommands
    if let Some(command) = args.command {
        return handle_subcommand(command, args.config).await;
    }

    // Default: run documentation generation
    let config = args.to_config();
    launch(&config).await
}

/// Handle CLI subcommands
async fn handle_subcommand(command: cli::Commands, config_path: Option<std::path::PathBuf>) -> Result<()> {
    match command {
        cli::Commands::SyncKnowledge { config, force } => {
            sync_knowledge(config.or(config_path), force).await
        }
        cli::Commands::SemanticDocs {
            rustdoc_json,
            output,
            receipt,
            repository,
            revision,
            open_ontologies_bin,
        } => {
            let receipt = semantic::run(semantic::SemanticDocsOptions {
                rustdoc_json,
                output,
                receipt,
                repository,
                revision,
                open_ontologies_bin,
            })?;
            println!(
                "semantic documentation: items={} documented={} references={} spans={} standing={} authority={}",
                receipt.item_count,
                receipt.documented_item_count,
                receipt.reference_count,
                receipt.source_span_count,
                receipt.standing,
                receipt.authority
            );
            Ok(())
        }
    }
}

/// Sync external knowledge sources
async fn sync_knowledge(config_path: Option<std::path::PathBuf>, force: bool) -> Result<()> {
    use integrations::KnowledgeSyncer;

    // Load configuration
    let config = if let Some(path) = config_path {
        config::Config::from_file(&path)?
    } else {
        // Try default location
        let default_path = std::path::PathBuf::from("litho.toml");
        if default_path.exists() {
            config::Config::from_file(&default_path)?
        } else {
            println!("⚠️  No configuration file found. Using defaults.");
            config::Config::default()
        }
    };

    // Create syncer
    let syncer = KnowledgeSyncer::new(config)?;

    // Check if sync is needed
    if !force && !syncer.should_sync()? {
        println!("✅ Knowledge cache is up to date. Use --force to sync anyway.");
        return Ok(());
    }

    // Perform sync
    syncer.sync_all().await?;

    Ok(())
}
