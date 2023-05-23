use std::{
    env,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::Duration,
};

use anyhow::{Context, Ok, Result};
use config::{Config, FileFormat};
use log::warn;
use reqwest::Url;
use secrecy::{ExposeSecret, SecretString};
use serde::Deserialize;
use teloxide::{
    net::Download,
    prelude::*,
    types::{FileMeta, MediaKind, MessageCommon, MessageKind},
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const CONFIG_PATH_ENV: &str = "CONFIG_PATH";
const TELEGRAM_BOT_API_URL_ENV: &str = "TELEGRAM_BOT_API_URL";

#[tokio::main]
async fn main() -> Result<()> {
    let app_config = read_config().context("Config read failed")?;

    run_bot(app_config).await;

    Ok(())
}

#[derive(Deserialize)]
struct AppConfig {
    bot_token: SecretString,
    channel_id: i64,
    media_directory: String,
}

struct AppState {
    config: AppConfig,
    media_group_page_numbers: Mutex<std::collections::HashMap<String, u32>>,
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

async fn run_bot(app_config: AppConfig) {
    env_logger::init();
    log::info!("Starting media downloader bot...");

    let client = teloxide::net::default_reqwest_settings()
        .timeout(Duration::from_secs(600))
        .build()
        .expect("Client creation failed");
    let mut tg = Bot::with_client(app_config.bot_token.expose_secret(), client);

    if let Some(url) = env::var_os(TELEGRAM_BOT_API_URL_ENV) {
        tg = tg.set_api_url(
            Url::parse(url.to_str().expect("Unicode string expected"))
                .expect("Bot api must be a url"),
        );
    }

    let tg = Arc::new(tg);

    let handler = Update::filter_channel_post().branch(
        dptree::filter(|msg: Message, app_state: Arc<AppState>| {
            app_state.config.channel_id == msg.chat.id.0
        })
        .endpoint(handle_media_message),
    );

    let app_state = Arc::new(AppState {
        config: app_config,
        media_group_page_numbers: Default::default(),
    });
    Dispatcher::builder(tg.clone(), handler)
        .dependencies(dptree::deps![app_state, tg.clone()])
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
    app_state: Arc<AppState>,
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

    let media_group_id = message.media_group_id().map(|s| s.to_owned());

    // increment page number
    let page_number = if let Some(media_group_id) = &media_group_id {
        let mut map = app_state.media_group_page_numbers.lock().unwrap();
        let page_number = map.entry(media_group_id.clone()).or_insert(0);
        *page_number += 1;
        Some(*page_number)
    } else {
        None
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
                &app_state.config.media_directory,
                photo.caption.as_deref(),
                "jpg",
                page_number,
            )
            .await
            .context("Failed download photo")?;
        }
        MediaKind::Video(video) => {
            download_and_save_file(
                bot,
                &video.video.file,
                &app_state.config.media_directory,
                video
                    .caption
                    .as_deref()
                    .or(video.video.file_name.as_deref()),
                "mp4",
                page_number,
            )
            .await
            .context("Failed download video")?;
        }
        MediaKind::Audio(audio) => {
            download_and_save_file(
                bot,
                &audio.audio.file,
                &app_state.config.media_directory,
                audio
                    .caption
                    .as_deref()
                    .or(audio.audio.file_name.as_deref()),
                "mp3",
                page_number,
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
    page_number: Option<u32>,
) -> Result<()> {
    let file = bot.get_file(file_meta.id.clone()).send().await?;
    let (filename, extension) = get_filename_and_extension(file_meta, file_name, ext, page_number);
    let mut file_path = PathBuf::from(media_directory);
    file_path.push(format!("{}.{}", filename, extension));

    tokio::fs::create_dir_all(&file_path.parent().expect("Parent missing"))
        .await
        .context("Create dir all failed")?;
    let mut dst = tokio::fs::File::create(&file_path)
        .await
        .context(format!("Failed to create file: {}", file_path.display()))?;
    if Path::new(&file.path).is_absolute() {
        let mut absolute_file = tokio::fs::File::open(file.path).await?;
        let mut buf = Vec::new();
        absolute_file.read_to_end(&mut buf).await?;
        dst.write_all(&buf).await?;
    } else if let Err(e) = bot.download_file(&file.path, &mut dst).await {
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
    page_number: Option<u32>,
) -> (String, String) {
    let stem = file_name
        .map(Path::new)
        .and_then(|p| p.file_stem().and_then(|s| s.to_str()))
        .unwrap_or("");

    let ext = file_name
        .map(Path::new)
        .and_then(|p| p.extension().and_then(|e| e.to_str()))
        .unwrap_or(default_ext);

    let title_prefix = if page_number.is_some() { "title:" } else { "" };
    let page_part = page_number.map_or_else(String::new, |num| format!("{{page:{}}}", num));

    let unique_id = &file_meta.unique_id;
    let filename = format!("{title_prefix}[{stem}]_{unique_id}{page_part}");

    (filename, ext.to_owned())
}
