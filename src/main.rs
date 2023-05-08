use std::{path::Path, sync::Arc, time::Duration};

use anyhow::{Context, Ok, Result};
use config::Config;
use log::warn;
use secrecy::{ExposeSecret, SecretString};
use serde::Deserialize;
use teloxide::{
    net::Download,
    types::{FileMeta, MediaKind, MessageKind, MessageCommon}, prelude::*,
};

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
    let config = Config::builder()
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
        dptree::filter(|msg: Message, config: Arc<AppConfig>| config.channel_id == msg.chat.id.0)
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

async fn handle_media_message(bot: &Bot, message: &Message, config: Arc<AppConfig>) -> Result<()> {
    match &message.kind {
        MessageKind::Common {
            0: MessageCommon { media_kind, .. },
            ..
        } => match media_kind {
            MediaKind::Photo(photo) => {
                let max_size = photo
                    .photo
                    .iter()
                    .max_by_key(|photo| photo.file.size)
                    .unwrap();

                download_and_save_file(
                    bot,
                    &max_size.file,
                    &config.media_directory,
                    photo.caption.as_deref(),
                    "jpg",
                )
                .await
                .context("Failed download photo")?;
            }
            MediaKind::Video(video) => {
                download_and_save_file(
                    bot,
                    &video.video.file,
                    &config.media_directory,
                    video
                        .caption
                        .as_deref()
                        .or(video.video.file_name.as_deref()),
                    "mp4",
                )
                .await
                .context("Failed download video")?;
            }
            MediaKind::Audio(audio) => {
                download_and_save_file(
                    bot,
                    &audio.audio.file,
                    &config.media_directory,
                    audio.caption.as_deref(),
                    "mp3",
                )
                .await
                .context("Failed download audio")?;
            }
            _ => (),
        },
        _ => (),
    };
    Ok(())
}

async fn download_and_save_file(
    bot: &Bot,
    file_meta: &FileMeta,
    media_directory: &str,
    file_name: Option<&str>,
    ext: &str,
) -> Result<()> {
    let file = bot.get_file(file_meta.id.clone()).send().await?;
    let (filename, extension) = if let Some(file_name) = file_name.map(Path::new) {
        match (file_name.file_stem(), file_name.extension()) {
            (Some(stem), Some(ext)) => (
                stem.to_str().expect("Bad filename"),
                ext.to_str().expect("Bad filename"),
            ),
            _ => (file_meta.unique_id.as_str(), ext),
        }
    } else {
        (file_meta.unique_id.as_str(), ext)
    };
    let file_path = format!("{}/{}.{}", media_directory, filename, extension);

    let mut dst = tokio::fs::File::create(&file_path).await?;
    if let Err(e) = bot.download_file(&file.path, &mut dst).await {
        log::error!("Failed to download file: {}", e);
    } else {
        log::info!("Downloaded and saved file: {}", file_path);
    }
    Ok(())
}
