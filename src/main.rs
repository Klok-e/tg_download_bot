use std::{
    env,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use anyhow::{Context, Ok, Result};
use config::{Config, FileFormat};
use log::warn;
use secrecy::{ExposeSecret, SecretString};
use serde::Deserialize;
use teloxide::{
    net::Download,
    prelude::*,
    types::{FileMeta, MediaKind, MessageCommon, MessageKind},
};

const CONFIG_PATH_ENV: &str = "CONFIG_PATH";

#[tokio::main]
async fn main() -> Result<()> {
    let app_config = read_config().context("Config read failed")?;

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
        .add_source(config::File::new(
            &env::var(CONFIG_PATH_ENV)
                .with_context(|| format!("{CONFIG_PATH_ENV} environment variable not set"))?,
            FileFormat::Toml,
        ))
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

    let handler = Update::filter_channel_post().branch(
        dptree::filter(|msg: Message, config: Arc<AppConfig>| config.channel_id == msg.chat.id.0)
            .endpoint(handle_media_message),
    );

    Dispatcher::builder(bot.clone(), handler)
        .dependencies(dptree::deps![app_config.clone(), bot.clone()])
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

async fn handle_media_message(
    bot: Arc<Bot>,
    message: Message,
    config: Arc<AppConfig>,
) -> Result<()> {
    let media_kind = if let MessageKind::Common {
        0: MessageCommon { media_kind, .. },
        ..
    } = &message.kind
    {
        media_kind
    } else {
        return Ok(());
    };
    match media_kind {
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
                audio
                    .caption
                    .as_deref()
                    .or(audio.audio.file_name.as_deref()),
                "mp3",
            )
            .await
            .context("Failed download audio")?;
        }
        _ => (),
    }
    Ok(())
}

async fn download_and_save_file(
    bot: Arc<Bot>,
    file_meta: &FileMeta,
    media_directory: &str,
    file_name: Option<&str>,
    ext: &str,
) -> Result<()> {
    let file = bot.get_file(file_meta.id.clone()).send().await?;
    let (filename, extension) = get_filename_and_extension(file_meta, file_name, ext);
    let mut file_path = PathBuf::from(media_directory);
    file_path.push(format!("{}.{}", filename, extension));

    tokio::fs::create_dir_all(&file_path.parent().expect("Parent missing"))
        .await
        .context("Create dir all failed")?;
    let mut dst = tokio::fs::File::create(&file_path)
        .await
        .context(format!("Failed to create file: {}", file_path.display()))?;
    if let Err(e) = bot.download_file(&file.path, &mut dst).await {
        log::error!("Failed to download file: {}", e);
    } else {
        log::info!("Downloaded and saved file: {}", file_path.display());
    }
    Ok(())
}

fn get_filename_and_extension(
    file_meta: &FileMeta,
    file_name: Option<&str>,
    default_ext: &str,
) -> (String, String) {
    let stem = if let Some(file_name) = file_name.map(Path::new) {
        file_name
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or(&file_meta.unique_id)
    } else {
        &file_meta.unique_id
    };

    let ext = if let Some(file_name) = file_name.map(Path::new) {
        file_name
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or(default_ext)
    } else {
        default_ext
    };

    let filename = format!("{}_{}", stem, file_meta.unique_id);

    (filename, ext.to_owned())
}
