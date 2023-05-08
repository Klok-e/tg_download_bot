use std::{fs, sync::Arc, time::Duration};

use anyhow::{Context, Ok, Result};
use config::{Config, ConfigBuilder, File};
use log::warn;
use secrecy::{ExposeSecret, SecretString};
use serde::Deserialize;
use teloxide::{
    net::Download,
    types::{Document, InputFile, MessageKind, PhotoSize},
};
use teloxide::{prelude::*, types::MessageCommon};
use tokio::runtime::Builder;

#[tokio::main]
async fn main() -> Result<()> {
    let app_config = read_config()?;

    run_bot(Arc::new(app_config)).await;

    Ok(())
}

#[derive(Deserialize)]
struct AppConfig {
    bot_token: SecretString,
    channel_id: i64,
    media_directory: String,
}

fn read_config() -> Result<AppConfig> {
    let mut config = Config::builder()
        .add_source(config::File::with_name("config"))
        .build()?;

    Ok(config
        .try_deserialize::<AppConfig>()
        .context("Failed to parse config values")?)
}

async fn run_bot(app_config: Arc<AppConfig>) {
    env_logger::init();
    log::info!("Starting media downloader bot...");

    let client = teloxide::net::default_reqwest_settings()
        .timeout(Duration::from_secs(120))
        .build()
        .expect("Client creation failed");
    let bot = Arc::new(Bot::with_client(
        app_config.bot_token.expose_secret(),
        client,
    ));

    let handler = Update::filter_message().branch(
        dptree::filter(|msg: Message, config: Arc<config::Config>| {
            app_config.channel_id == msg.chat.id.0
        })
        .endpoint(handle_media_message),
    );

    Dispatcher::builder(bot.clone(), handler)
        .dependencies(dptree::deps![app_config.clone()])
        .default_handler(|upd| async move {
            warn!("unhandled update: {:?}", upd);
        })
        .error_handler(LoggingErrorHandler::with_custom_text(
            "an error has occurred in the dispatcher",
        ))
        .build()
        .dispatch()
        .await;
}

async fn handle_media_message(bot: &Bot, message: &Message, media_directory: &str) -> Result<()> {
    match &message.kind {
        MessageKind::Common {
            0: MessageCommon { media_kind, .. },
            ..
        } => match media_kind {
            teloxide::types::MediaKind::Photo { 0: photo, .. } => {
                let max_size = photo
                    .photo
                    .iter()
                    .max_by_key(|photo| photo.file.size)
                    .unwrap();
                download_and_save_file(bot, &max_size.file.id, media_directory).await;
            }
            teloxide::types::MediaKind::Video { 0: video, .. } => {
                download_and_save_file(bot, video, media_directory).await;
            }
            teloxide::types::MediaKind::Document { 0: document, .. } => {
                download_and_save_file(bot, document, media_directory).await;
            }
            _ => (),
        },
        _ => (),
    };
    Ok(())
}

async fn download_and_save_file(bot: &Bot, file_id: &str, media_directory: &str) -> Result<()> {
    let file = bot.get_file(file_id).send().await?;
    let file_path = format!("{}/{}", media_directory, file_id);

    let mut dst = tokio::fs::File::create(&file_path).await?;
    if let Err(e) = bot.download_file(&file.path, &mut dst).await {
        log::error!("Failed to download file: {}", e);
    } else {
        log::info!("Downloaded and saved file: {}", file_path);
    }
    Ok(())
}
