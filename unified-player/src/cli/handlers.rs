use crate::client::listenbrainz_import::parse_listenbrainz_playlist;
use crate::{auth::AuthConfig, client};

use super::{
    config, init_cli, start_socket, AlbumId, Command, ContextType, EditAction, GetRequest,
    IdOrName, ItemType, Key, PlaylistCommand, PlaylistId, Request, Response, TrackId,
    MAX_REQUEST_SIZE,
};
use anyhow::{Context, Result};
use clap::{ArgMatches, Id};
use clap_complete::{generate, Shell};
use std::collections::HashMap;
#[cfg(feature = "private-capture")]
use std::io::IsTerminal as _;
use std::io::Write as _;
use std::net::UdpSocket;

fn receive_response(socket: &UdpSocket) -> Result<Response> {
    // read response from the server's socket, which can be split into
    // smaller chunks of data
    let mut data = Vec::new();
    let mut buf = [0; 4096];
    loop {
        let (n_bytes, _) = socket.recv_from(&mut buf)?;
        if n_bytes == 0 {
            // end of chunk
            break;
        }
        data.extend_from_slice(&buf[..n_bytes]);
    }

    Ok(serde_json::from_slice(&data)?)
}

fn get_id_or_name(args: &ArgMatches) -> IdOrName {
    try_get_id_or_name(args).expect("id_or_name group is required")
}

fn try_get_id_or_name(args: &ArgMatches) -> Option<IdOrName> {
    match args.get_one::<Id>("id_or_name")?.as_str() {
        "name" => Some(IdOrName::Name(
            args.get_one::<String>("name")
                .expect("name should be specified")
                .to_owned(),
        )),
        "id" => Some(IdOrName::Id(
            args.get_one::<String>("id")
                .expect("id should be specified")
                .to_owned(),
        )),
        id => panic!("unknown id: {id}"),
    }
}

fn cli_youtube_projection_scope(configs: &config::Configs) -> (String, u64) {
    let account_id = config::AccountRegistry::load(&configs.config_folder)
        .ok()
        .and_then(|registry| {
            registry
                .active_id(config::ActiveProvider::YouTubeMusic)
                .map(str::to_owned)
        })
        .unwrap_or_else(|| "unknown".to_owned());
    // The daemon owns the interactive epoch.  CLI invocations are isolated
    // sessions, so zero is an explicit, stable CLI epoch rather than a
    // fabricated account-switch counter.
    (account_id, 0)
}

fn unified_remove_entry_ids(
    playlist: &crate::state::UnifiedPlaylist,
    entry_id: Option<u64>,
    item_id: Option<&str>,
    all_occurrences: bool,
) -> Result<Vec<crate::state::PlaylistEntryId>> {
    if let Some(entry_id) = entry_id {
        anyhow::ensure!(
            !all_occurrences,
            "--entry-id cannot be combined with --all-occurrences"
        );
        anyhow::ensure!(
            item_id.is_none(),
            "--entry-id cannot be combined with --item-id"
        );
        anyhow::ensure!(
            playlist
                .items
                .iter()
                .any(|item| item.entry_id.0 == entry_id),
            "unified playlist occurrence not found: {entry_id}"
        );
        return Ok(vec![crate::state::PlaylistEntryId(entry_id)]);
    }

    let item_id = item_id.context(
        "provide --entry-id for exact removal, or pair --item-id with --all-occurrences",
    )?;
    anyhow::ensure!(
        all_occurrences,
        "raw --item-id removal is disabled by default; pass --all-occurrences explicitly"
    );
    let entry_ids = playlist
        .items
        .iter()
        .filter(|item| item.media_id.raw_id == item_id)
        .map(|item| item.entry_id)
        .collect::<Vec<_>>();
    anyhow::ensure!(
        !entry_ids.is_empty(),
        "no matching playlist occurrences found"
    );
    Ok(entry_ids)
}

fn handle_get_subcommand(args: &ArgMatches) -> Request {
    let (cmd, args) = args.subcommand().expect("playback subcommand is required");

    let request = match cmd {
        "key" => {
            let key = args
                .get_one::<Key>("key")
                .expect("key is required")
                .to_owned();
            Request::Get(GetRequest::Key(key))
        }
        "item" => {
            let item_type = args
                .get_one::<ItemType>("item_type")
                .expect("context_type is required")
                .to_owned();
            let id_or_name = get_id_or_name(args);
            Request::Get(GetRequest::Item(item_type, id_or_name))
        }
        _ => unreachable!(),
    };

    request
}

fn handle_playback_subcommand(args: &ArgMatches) -> Result<Request> {
    let (cmd, args) = args.subcommand().expect("playback subcommand is required");
    let command = match cmd {
        "start" => match args.subcommand() {
            Some(("track", args)) => Command::StartTrack(get_id_or_name(args)),
            Some(("context", args)) => {
                let context_type = args
                    .get_one::<ContextType>("context_type")
                    .expect("context_type is required")
                    .to_owned();
                let shuffle = args.get_flag("shuffle");

                let id_or_name = get_id_or_name(args);
                Command::StartContext {
                    context_type,
                    id_or_name,
                    shuffle,
                }
            }
            Some(("liked", args)) => {
                let limit = *args
                    .get_one::<usize>("limit")
                    .expect("limit should have a default value");
                let random = args.get_flag("random");
                Command::StartLikedTracks { limit, random }
            }
            Some(("radio", args)) => {
                let item_type = args
                    .get_one::<ItemType>("item_type")
                    .expect("item_type is required")
                    .to_owned();
                let id_or_name = get_id_or_name(args);
                Command::StartRadio(item_type, id_or_name)
            }
            _ => {
                anyhow::bail!("invalid command!");
            }
        },
        "play-pause" => Command::PlayPause,
        "play" => Command::Play,
        "pause" => Command::Pause,
        "next" => Command::Next,
        "previous" => Command::Previous,
        "shuffle" => Command::Shuffle,
        "repeat" => Command::Repeat,
        "volume" => {
            let percent = args
                .get_one::<i8>("percent")
                .expect("percent arg is required");
            let offset = args.get_flag("offset");
            Command::Volume {
                percent: *percent,
                is_offset: offset,
            }
        }
        "seek" => {
            let position_offset_ms = args
                .get_one::<i64>("position_offset_ms")
                .expect("position_offset_ms is required");
            Command::Seek(*position_offset_ms)
        }
        _ => unreachable!(),
    };

    Ok(Request::Playback(command))
}

fn handle_youtube_subcommand(args: &ArgMatches, configs: &config::Configs) -> Result<()> {
    let (subcommand, subcommand_args) = args
        .subcommand()
        .context("youtube subcommand is required")?;

    match subcommand {
        #[cfg(feature = "private-capture")]
        "debug-capture" => return super::private_capture::handle(subcommand_args, configs),
        #[cfg(feature = "private-capture")]
        "debug" => return handle_youtube_debug(subcommand_args, configs),
        "probe" => {
            let video_id = subcommand_args
                .get_one::<String>("video_id")
                .expect("video-id is required");
            let allow_browser_fallback = subcommand_args.get_flag("allow_browser_fallback");
            let probe_client = client::YouTubeProbeClient::from_cli(
                subcommand_args
                    .get_one::<String>("client")
                    .expect("client has a default"),
            );
            let decoder_chunk_size = client::YouTubeProbeDecoderChunkSize::from_cli(
                subcommand_args
                    .get_one::<String>("decoder_chunk_size")
                    .expect("decoder chunk size has a default"),
            );
            let report =
                tokio::runtime::Runtime::new()?.block_on(client::probe_youtube_playback_for_video(
                    configs,
                    video_id,
                    allow_browser_fallback,
                    probe_client,
                    decoder_chunk_size,
                ));
            if subcommand_args.get_flag("json") {
                println!("{}", serde_json::to_string(&report)?);
            } else {
                println!("{report}");
            }
            if !report.is_success() {
                anyhow::bail!(
                    "YouTube probe failed ({})",
                    report.error_category().unwrap_or("unknown")
                );
            }
        }
        "status" => {
            let status = configs.youtube_music_auth_status();
            println!("YouTube Music auth: {}", status.label());
            if let Some(path) = status.credential_path {
                println!("Credential path: {}", path.display());
            }
            if subcommand_args.get_flag("check") {
                let rt = tokio::runtime::Runtime::new()?;
                let library = rt
                    .block_on(client::check_youtube_auth())
                    .context("check YouTube Music authentication")?;
                println!(
                    "Authenticated library response: {} playlists, {} albums, {} artists",
                    library.playlists.len(),
                    library.albums.len(),
                    library.artists.len()
                );
                if library.playlists.is_empty()
                    && library.albums.is_empty()
                    && library.artists.is_empty()
                    && library.errors.is_empty()
                {
                    println!("Credential was accepted, but the account returned an empty library.");
                }
                for error in library.errors {
                    println!("Library warning: {error}");
                }
                let video_id = subcommand_args.get_one::<String>("video_id");
                let playback = if subcommand_args.get_flag("transport_diagnostic") {
                    rt.block_on(client::diagnose_youtube_media_transport_for_video(
                        video_id.expect("transport diagnostic requires a video ID"),
                    ))
                } else if subcommand_args.get_flag("audio_output") {
                    rt.block_on(client::check_youtube_playback_output_for_video(
                        video_id.map(String::as_str),
                    ))
                } else if let Some(video_id) = video_id {
                    rt.block_on(client::check_youtube_playback_auth_for_video(video_id))
                } else {
                    rt.block_on(client::check_youtube_playback_auth())
                }
                .context("check YouTube playback validation")?;
                if subcommand_args.get_flag("transport_diagnostic") {
                    println!("Media transport diagnostic: {playback}");
                } else {
                    println!("Playback validation response: {playback}");
                }
            }
        }
        "auth" => {
            let configured = match configs.app_config.youtube.auth_type {
                config::YouTubeMusicAuthType::OAuth => "oauth",
                config::YouTubeMusicAuthType::Browser
                | config::YouTubeMusicAuthType::Unauthenticated => "browser",
            };
            let auth_type = subcommand_args
                .get_one::<String>("auth_type")
                .map_or(configured, String::as_str);
            let path = match auth_type {
                "browser" => configs.youtube_music_cookie_path(),
                "oauth" => configs.youtube_music_oauth_path(),
                _ => unreachable!("clap restricts YouTube auth types"),
            };

            println!("YouTube Music {auth_type} authentication setup");
            if auth_type == "browser" {
                println!("Recommended: run `unified-player youtube browser-login`.");
                println!(
                    "It uses a dedicated browser profile, saves the session to {}, and refreshes rotating cookies automatically.",
                    path.display()
                );
                println!(
                    "Manual Cookie-header import remains available from the YouTube Music section in Settings."
                );
            } else {
                println!("Run `unified-player youtube login` with a Google TVs and Limited Input OAuth client.");
                println!(
                    "The refresh token is saved to {} and access-token refresh is automatic.",
                    path.display()
                );
            }
            println!("Run `unified-player youtube status` to verify the credential file.");
        }
        "login" => {
            let client_id = youtube_oauth_value(
                subcommand_args,
                "client_id",
                "UNIFIED_PLAYER_YOUTUBE_OAUTH_CLIENT_ID",
                "YOUTUI_OAUTH_CLIENT_ID",
            )?;
            let client_secret = youtube_oauth_value(
                subcommand_args,
                "client_secret",
                "UNIFIED_PLAYER_YOUTUBE_OAUTH_CLIENT_SECRET",
                "YOUTUI_OAUTH_CLIENT_SECRET",
            )?;
            let rt = tokio::runtime::Runtime::new()?;
            let login = rt
                .block_on(client::begin_youtube_oauth_login(client_id))
                .context("start YouTube Music Google sign-in")?;
            let verification_url = login.verification_url().to_string();
            println!("Open this Google verification page:\n{verification_url}");
            if !subcommand_args.get_flag("no_open") {
                open::that_in_background(&verification_url);
            }
            println!("Complete sign-in in the browser, then press Enter here.");
            let mut input = String::new();
            std::io::stdin()
                .read_line(&mut input)
                .context("wait for YouTube Music Google sign-in")?;
            let token_path = configs.youtube_music_oauth_path();
            rt.block_on(login.finish(client_secret, &token_path))?;
            config::save_app_config_override(&configs.config_folder, "youtube.auth_type", "OAuth")?;
            println!(
                "YouTube Music Google sign-in saved to {}. Future access-token refreshes are automatic.",
                token_path.display()
            );
        }
        "browser-login" => {
            let browser = subcommand_args
                .get_one::<std::path::PathBuf>("browser")
                .cloned();
            let close_browser = !subcommand_args.get_flag("keep_open");
            let rt = tokio::runtime::Runtime::new()?;
            let mut session = rt
                .block_on(client::begin_youtube_browser_login(browser))
                .context("open dedicated YouTube sign-in browser")?;
            println!(
                "A dedicated YouTube Music browser profile is open at {}.",
                session.profile_path().display()
            );
            println!(
                "Sign in to Google there. This command will detect the completed sign-in automatically."
            );
            print!("Waiting for the signed-in browser session... ");
            std::io::stdout().flush()?;
            let cookie_path = configs.youtube_music_cookie_path();
            let cancellation = tokio_util::sync::CancellationToken::new();
            let cookie_count = rt
                .block_on(session.wait_for_sign_in_and_save(
                    &cookie_path,
                    close_browser,
                    &cancellation,
                ))
                .context("import dedicated YouTube browser session")?;
            let Some(cookie_count) = cookie_count else {
                println!("cancelled.");
                return Ok(());
            };
            session.promote_to_active_profile(&configs.config_folder)?;
            drop(session);
            config::save_app_config_override(
                &configs.config_folder,
                "youtube.auth_type",
                "Browser",
            )?;
            println!(
                "done. Saved {cookie_count} YouTube session cookies to {}. The dedicated profile will refresh them automatically.",
                cookie_path.display()
            );
        }
        "like" => {
            let video_id = subcommand_args
                .get_one::<String>("video_id")
                .expect("video-id is required");
            let liked = !subcommand_args.get_flag("unlike");
            tokio::runtime::Runtime::new()?
                .block_on(client::youtube_rate_song(video_id, liked))
                .context("update YouTube Music like status")?;
            println!("{} {video_id}", if liked { "Liked" } else { "Unliked" });
        }
        "playlist" => {
            let (action, action_args) = subcommand_args
                .subcommand()
                .context("youtube playlist subcommand is required")?;
            let rt = tokio::runtime::Runtime::new()?;
            match action {
                "create" => {
                    let name = action_args
                        .get_one::<String>("name")
                        .expect("name is required");
                    let public = action_args.get_flag("public");
                    let id = rt
                        .block_on(client::youtube_create_playlist(name, public))
                        .context("create YouTube Music playlist")?;
                    println!("{id}");
                }
                "add" => {
                    let playlist_id = action_args
                        .get_one::<String>("playlist_id")
                        .expect("playlist-id is required");
                    let video_id = action_args
                        .get_one::<String>("video_id")
                        .expect("video-id is required");
                    rt.block_on(client::youtube_add_video_to_playlist(playlist_id, video_id))
                        .context("add video to YouTube Music playlist")?;
                    println!("Added {video_id} to {playlist_id}");
                }
                "remove" => {
                    let playlist_id = action_args
                        .get_one::<String>("playlist_id")
                        .expect("playlist-id is required");
                    let set_video_id = action_args
                        .get_one::<String>("set_video_id")
                        .expect("set-video-id is required");
                    rt.block_on(client::youtube_remove_video_from_playlist(
                        playlist_id,
                        set_video_id,
                    ))
                    .context("remove video from YouTube Music playlist")?;
                    println!("Removed {set_video_id} from {playlist_id}");
                }
                "delete" => {
                    let playlist_id = action_args
                        .get_one::<String>("playlist_id")
                        .expect("playlist-id is required");
                    rt.block_on(client::youtube_delete_playlist(playlist_id))
                        .context("delete YouTube Music playlist")?;
                    println!("Deleted {playlist_id}");
                }
                _ => unreachable!("clap restricts YouTube playlist subcommands"),
            }
        }
        _ => unreachable!("clap restricts YouTube subcommands"),
    }

    Ok(())
}

fn handle_demo_subcommand(args: &ArgMatches, configs: &config::Configs) -> Result<()> {
    let (subcommand, subcommand_args) = args.subcommand().context("demo command is required")?;
    match subcommand {
        "screen" => {
            let screen = crate::ui::PreviewScreen::from_cli(
                subcommand_args
                    .get_one::<String>("screen")
                    .expect("screen is required"),
            )?;
            let scenario = crate::ui::PreviewScenario::from_cli(
                subcommand_args
                    .get_one::<String>("scenario")
                    .expect("scenario has a default"),
            )?;
            if subcommand_args.get_flag("interactive") {
                return crate::ui::run_screen_preview_interactive(configs, screen, scenario);
            }
            let sizes = match subcommand_args.get_many::<String>("size") {
                Some(values) => values
                    .map(|value| crate::ui::preview_size_from_cli(value))
                    .collect::<Result<Vec<_>>>()?,
                None => vec![(80, 24), (120, 35), (180, 49)],
            };
            let color = match subcommand_args
                .get_one::<String>("color")
                .map(String::as_str)
            {
                Some("always") => true,
                Some("never") => false,
                _ => std::io::IsTerminal::is_terminal(&std::io::stdout()),
            };
            print!(
                "{}",
                crate::ui::render_screen_preview(configs, screen, scenario, &sizes, color)?
            );
        }
        "welcome" => {
            let scenario = crate::ui::WelcomeDemoScenario::from_cli(
                subcommand_args
                    .get_one::<String>("scenario")
                    .expect("scenario has a default"),
            )?;
            let step = crate::ui::welcome_demo_step_from_cli(
                subcommand_args
                    .get_one::<String>("step")
                    .expect("step has a default"),
            )?;
            if subcommand_args.get_flag("interactive") {
                let layout = crate::ui::welcome_demo_layout_from_cli(
                    subcommand_args
                        .get_one::<String>("layout")
                        .expect("layout has a default"),
                )?;
                return crate::ui::run_welcome_demo_interactive(configs, scenario, step, layout);
            }
            let width = *subcommand_args
                .get_one::<u16>("width")
                .expect("width has a default");
            let height = *subcommand_args
                .get_one::<u16>("height")
                .expect("height has a default");
            println!(
                "{}",
                crate::ui::render_welcome_demo_layout(
                    configs,
                    scenario,
                    step,
                    width,
                    height,
                    crate::ui::welcome_demo_layout_from_cli(
                        subcommand_args
                            .get_one::<String>("layout")
                            .expect("layout has a default")
                    )?
                )?
            );
        }
        _ => unreachable!(),
    }
    Ok(())
}

fn youtube_oauth_value(
    args: &ArgMatches,
    argument: &str,
    primary_env: &str,
    compatible_env: &str,
) -> Result<String> {
    args.get_one::<String>(argument)
        .cloned()
        .or_else(|| std::env::var(primary_env).ok())
        .or_else(|| std::env::var(compatible_env).ok())
        .filter(|value| !value.trim().is_empty())
        .with_context(|| {
            format!(
                "missing --{} (or {primary_env}); create a Google OAuth client for TVs and Limited Input devices first",
                argument.replace('_', "-")
            )
        })
}

fn handle_unified_subcommand(args: &ArgMatches, configs: &config::Configs) -> Result<()> {
    let (subcommand, subcommand_args) = args
        .subcommand()
        .context("unified subcommand is required")?;
    let mut data = crate::state::AppData::new(&configs.config_folder, &configs.cache_folder);
    match subcommand {
        "list" => {
            for playlist in &data.unified_playlists {
                println!(
                    "{}\t{}\t{} items",
                    playlist.id,
                    playlist.name,
                    playlist.items.len()
                );
            }
        }
        "new" => {
            let name = subcommand_args
                .get_one::<String>("name")
                .expect("name is required")
                .to_owned();
            let id = format!(
                "local-{}",
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .context("read system clock")?
                    .as_nanos()
            );
            data.upsert_unified_playlist(crate::state::UnifiedPlaylist {
                id: id.clone(),
                name,
                items: Vec::new(),
                updated_at: id
                    .strip_prefix("local-")
                    .and_then(|value| value.parse().ok())
                    .unwrap_or_default(),
                next_entry_id: 1,
            })?;
            println!("{id}");
        }
        "delete" => {
            let id = subcommand_args
                .get_one::<String>("id")
                .expect("id is required");
            data.delete_unified_playlist(id)?;
        }
        "add" => {
            let playlist_id = subcommand_args
                .get_one::<String>("playlist_id")
                .expect("playlist-id is required")
                .clone();
            let provider = subcommand_args
                .get_one::<String>("provider")
                .expect("provider is required");
            let item_id = subcommand_args
                .get_one::<String>("item_id")
                .expect("item-id is required")
                .clone();
            let title = subcommand_args
                .get_one::<String>("title")
                .expect("title is required")
                .clone();
            let artists = subcommand_args
                .get_one::<String>("artists")
                .cloned()
                .unwrap_or_default();
            let duration_ms = subcommand_args.get_one::<u64>("duration_ms").copied();
            let video = subcommand_args.get_flag("video");
            let media_id = crate::state::MediaId {
                provider: if provider == "spotify" {
                    crate::state::Provider::Spotify
                } else {
                    crate::state::Provider::YouTubeMusic
                },
                kind: if video {
                    crate::state::MediaKind::Video
                } else {
                    crate::state::MediaKind::Track
                },
                raw_id: item_id,
            };
            data.append_unified_playlist_items(
                &playlist_id,
                vec![crate::state::UnifiedPlaylistItem {
                    media_id,
                    title,
                    artists,
                    duration_ms,
                    provider_url: None,
                    ..crate::state::UnifiedPlaylistItem::default()
                }],
            )?;
        }
        "remove" => {
            let playlist_id = subcommand_args
                .get_one::<String>("playlist_id")
                .expect("playlist-id is required");
            let playlist = data
                .unified_playlists
                .iter()
                .find(|playlist| playlist.id == *playlist_id)
                .context("unified playlist not found")?;
            let entry_ids = unified_remove_entry_ids(
                playlist,
                subcommand_args.get_one::<u64>("entry_id").copied(),
                subcommand_args
                    .get_one::<String>("item_id")
                    .map(String::as_str),
                subcommand_args.get_flag("all_occurrences"),
            )?;
            data.remove_unified_playlist_items(playlist_id, &entry_ids)?;
        }
        "export" => {
            let output = subcommand_args
                .get_one::<String>("output")
                .expect("output is required");
            let format = subcommand_args
                .get_one::<String>("format")
                .expect("format has a default");
            if format == "json" {
                data.export_unified_playlists(std::path::Path::new(output))?;
            } else {
                let playlist_id = subcommand_args
                    .get_one::<String>("playlist_id")
                    .context("--playlist-id is required for JSPF export")?;
                let playlist = data
                    .unified_playlists
                    .iter()
                    .find(|playlist| playlist.id == *playlist_id)
                    .context("unified playlist not found")?;
                let value = playlist.to_jspf_value();
                if let Some(parent) = std::path::Path::new(output).parent() {
                    std::fs::create_dir_all(parent)?;
                }
                serde_json::to_writer_pretty(std::fs::File::create(output)?, &value)?;
            }
            println!("{output}");
        }
        "link" => {
            let playlist_id = subcommand_args
                .get_one::<String>("playlist_id")
                .expect("playlist-id is required")
                .clone();
            anyhow::ensure!(
                data.unified_playlists
                    .iter()
                    .any(|playlist| playlist.id == playlist_id),
                "unified playlist not found: {playlist_id}"
            );
            let mut link = data
                .playlist_links
                .iter()
                .find(|link| link.unified_playlist_id == playlist_id)
                .cloned()
                .unwrap_or_else(|| crate::state::PlaylistLink {
                    unified_playlist_id: playlist_id,
                    ..crate::state::PlaylistLink::default()
                });
            if let Some(id) = subcommand_args.get_one::<String>("spotify_id") {
                link.spotify_playlist_id = Some(id.clone());
                let (account_id, account_epoch) =
                    config::AccountRegistry::load(&configs.config_folder)
                        .ok()
                        .and_then(|registry| {
                            registry
                                .active_id(config::ActiveProvider::Spotify)
                                .map(str::to_owned)
                        })
                        .map_or_else(|| ("unknown".to_owned(), 0), |account_id| (account_id, 0));
                link.upsert_projection(crate::state::PlaylistProjectionState::pending(
                    crate::state::PlaylistProjectionTarget {
                        provider: crate::state::Provider::Spotify,
                        account_id,
                        account_epoch,
                        playlist_id: id.clone(),
                    },
                ));
            }
            if let Some(id) = subcommand_args.get_one::<String>("youtube_id") {
                link.youtube_playlist_id = Some(id.clone());
                let (account_id, account_epoch) = cli_youtube_projection_scope(configs);
                link.upsert_projection(crate::state::PlaylistProjectionState::pending(
                    crate::state::PlaylistProjectionTarget {
                        provider: crate::state::Provider::YouTubeMusic,
                        account_id,
                        account_epoch,
                        playlist_id: id.clone(),
                    },
                ));
            }
            if let Some(id) = subcommand_args.get_one::<String>("listenbrainz_id") {
                link.listenbrainz_playlist_id = Some(id.clone());
            }
            link.updated_at = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .context("read system clock")?
                .as_secs();
            data.upsert_playlist_link(link)?;
        }
        "project" => {
            let playlist_id = subcommand_args
                .get_one::<String>("playlist_id")
                .expect("playlist-id is required");
            let provider = subcommand_args
                .get_one::<String>("provider")
                .expect("provider is required");
            let target_playlist_id = subcommand_args
                .get_one::<String>("target_playlist_id")
                .expect("target-playlist-id is required");
            anyhow::ensure!(
                provider == "youtube",
                "only YouTube projection is supported"
            );
            let mut playlist = data
                .unified_playlists
                .iter()
                .find(|playlist| playlist.id == *playlist_id)
                .context("unified playlist not found")?
                .clone();
            let (account_id, account_epoch) = cli_youtube_projection_scope(configs);
            let previous_projection = data
                .playlist_links
                .iter()
                .find(|link| link.unified_playlist_id == playlist.id)
                .and_then(|link| {
                    link.projection_for(
                        crate::state::Provider::YouTubeMusic,
                        &account_id,
                        account_epoch,
                        target_playlist_id,
                    )
                });
            let previous_snapshot =
                previous_projection.and_then(|projection| projection.local_revision.clone());
            let previous_youtube_snapshot =
                previous_projection.and_then(|projection| projection.remote_revision.clone());
            let remote_state = if subcommand_args.get_flag("check_remote") {
                let tracks = tokio::runtime::Runtime::new()?
                    .block_on(client::youtube_playlist_tracks(target_playlist_id))?;
                Some((
                    crate::state::UnifiedPlaylist::youtube_tracks_snapshot_hash(&tracks),
                    tracks,
                ))
            } else {
                None
            };
            let resolution = subcommand_args
                .get_one::<String>("resolution")
                .map(String::as_str);
            if resolution == Some("merge") {
                let (_, remote_tracks) = remote_state
                    .as_ref()
                    .context("--resolution merge requires --check-remote")?;
                for track in remote_tracks {
                    let media_id = crate::state::MediaId {
                        provider: crate::state::Provider::YouTubeMusic,
                        kind: if track.is_video {
                            crate::state::MediaKind::Video
                        } else {
                            crate::state::MediaKind::Track
                        },
                        raw_id: track.id.clone(),
                    };
                    if !playlist.items.iter().any(|item| item.media_id == media_id) {
                        playlist.items.push(crate::state::UnifiedPlaylistItem {
                            media_id,
                            title: track.name.clone(),
                            artists: track.artists.clone(),
                            duration_ms: None,
                            provider_url: Some(format!(
                                "https://music.youtube.com/watch?v={}",
                                track.id
                            )),
                            ..crate::state::UnifiedPlaylistItem::default()
                        });
                    }
                }
                if subcommand_args.get_flag("apply") {
                    playlist.normalize_entry_ids()?;
                    let stored = data
                        .unified_playlists
                        .iter_mut()
                        .find(|stored| stored.id == playlist.id)
                        .context("unified playlist not found")?;
                    *stored = playlist.clone();
                }
                println!("- resolution: merged remote-only YouTube items into the local plan");
            }
            let local_snapshot = playlist.snapshot_hash();
            let search_runtime = if resolution == Some("match") {
                Some(tokio::runtime::Runtime::new()?)
            } else {
                None
            };
            let remote_candidates = remote_state
                .as_ref()
                .map(|(_, tracks)| tracks.clone())
                .unwrap_or_default();
            let projection = resolve_projection_items(&playlist, resolution, |item| {
                if let Some((remote, _)) =
                    crate::state::best_youtube_match(item, &remote_candidates)
                {
                    return Ok(vec![remote.clone()]);
                }
                let runtime = search_runtime
                    .as_ref()
                    .expect("match resolution initializes a search runtime");
                let query = format!("{} {}", item.title, item.artists);
                let results = runtime
                    .block_on(client::youtube_search_for_projection(&query))
                    .with_context(|| format!("search YouTube for '{}'", item.title))?;
                Ok(results.songs.into_iter().chain(results.videos).collect())
            })?;
            let ProjectionResolution {
                desired_youtube_ids: youtube_items,
                unresolved,
                matched,
                accepted_mappings,
            } = projection;
            let youtube_items_to_append = remote_state.as_ref().map_or_else(
                || youtube_items.clone(),
                |(_, remote_tracks)| {
                    let remote_ids = remote_tracks
                        .iter()
                        .map(|track| track.id.clone())
                        .collect::<Vec<_>>();
                    missing_youtube_ids(&youtube_items, &remote_ids)
                },
            );
            println!(
                "Unified playlist '{}' -> YouTube playlist {} (dry run: {})",
                playlist.name,
                target_playlist_id,
                if subcommand_args.get_flag("apply") {
                    "no"
                } else {
                    "yes"
                }
            );
            println!(
                "Would append {} YouTube item(s); {} unresolved item(s)",
                youtube_items_to_append.len(),
                unresolved.len()
            );
            for matched in &matched {
                println!("- matched: {matched}");
            }
            let local_snapshot_conflict = previous_snapshot
                .as_deref()
                .is_some_and(|previous| previous != local_snapshot);
            if let Some(previous_snapshot) = &previous_snapshot {
                if previous_snapshot.as_str() == local_snapshot {
                    println!("- local snapshot matches the last projection");
                } else {
                    println!("- conflict: local playlist changed since the last projection");
                }
            } else {
                println!("- no previous local projection snapshot is recorded");
            }
            if let Some((remote_snapshot, remote_tracks)) = &remote_state {
                let remote_ids = remote_tracks
                    .iter()
                    .map(|track| track.id.clone())
                    .collect::<Vec<_>>();
                let missing = youtube_items
                    .iter()
                    .filter(|id| !remote_ids.contains(id))
                    .count();
                let extra = remote_ids
                    .iter()
                    .filter(|id| !youtube_items.contains(id))
                    .count();
                println!(
                    "Remote target: {} item(s), {} missing, {} target-only, order {}",
                    remote_ids.len(),
                    missing,
                    extra,
                    if remote_ids == youtube_items {
                        "matches"
                    } else {
                        "differs"
                    }
                );
                if let Some(previous_youtube_snapshot) = &previous_youtube_snapshot {
                    if previous_youtube_snapshot == remote_snapshot {
                        println!("- remote snapshot matches the last projection");
                    } else {
                        println!("- conflict: remote target changed since the last projection");
                    }
                } else {
                    println!("- no previous remote projection snapshot is recorded");
                }
            }
            for item in &unresolved {
                println!("- unresolved: {item}");
            }
            ensure_projection_resolution_complete(
                resolution,
                subcommand_args.get_flag("apply"),
                &unresolved,
            )?;
            let remote_snapshot_conflict =
                remote_state.as_ref().is_some_and(|(remote_snapshot, _)| {
                    previous_youtube_snapshot
                        .as_deref()
                        .is_some_and(|previous| previous != remote_snapshot)
                });
            if subcommand_args.get_flag("apply") {
                anyhow::ensure!(
                    subcommand_args.get_flag("force")
                        || resolution.is_some()
                        || (!local_snapshot_conflict && !remote_snapshot_conflict),
                    "projection snapshots conflict; review the dry run, choose --resolution local|merge, or pass --force"
                );
                let failures = tokio::runtime::Runtime::new()?.block_on(
                    client::youtube_append_playlist_items(
                        target_playlist_id,
                        &youtube_items_to_append,
                    ),
                )?;
                for failure in &failures {
                    println!("- failed: {failure}");
                }
                let mut link = data
                    .playlist_links
                    .iter()
                    .find(|link| link.unified_playlist_id == playlist.id)
                    .cloned()
                    .unwrap_or_else(|| crate::state::PlaylistLink {
                        unified_playlist_id: playlist.id.clone(),
                        ..crate::state::PlaylistLink::default()
                    });
                link.youtube_playlist_id = Some(target_playlist_id.clone());
                let mut projection = link
                    .projection_for(
                        crate::state::Provider::YouTubeMusic,
                        &account_id,
                        account_epoch,
                        target_playlist_id,
                    )
                    .cloned()
                    .unwrap_or_else(|| {
                        crate::state::PlaylistProjectionState::pending(
                            crate::state::PlaylistProjectionTarget {
                                provider: crate::state::Provider::YouTubeMusic,
                                account_id: account_id.clone(),
                                account_epoch,
                                playlist_id: target_playlist_id.clone(),
                            },
                        )
                    });
                projection.local_revision = Some(local_snapshot);
                let accepted_at = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|duration| duration.as_secs())
                    .unwrap_or_default();
                for mut mapping in accepted_mappings {
                    mapping.accepted_at = accepted_at;
                    if let Some(existing) = projection
                        .mappings
                        .iter_mut()
                        .find(|existing| existing.local_entry_id == mapping.local_entry_id)
                    {
                        *existing = mapping;
                    } else {
                        projection.mappings.push(mapping);
                    }
                }
                if resolution == Some("match") {
                    projection.acknowledged_intents.clear();
                }
                let mut remote_tracks_for_projection = None;
                if subcommand_args.get_flag("check_remote") {
                    match tokio::runtime::Runtime::new()?
                        .block_on(client::youtube_playlist_tracks(target_playlist_id))
                    {
                        Ok(tracks) => {
                            projection.remote_revision =
                                Some(crate::state::UnifiedPlaylist::youtube_tracks_snapshot_hash(
                                    &tracks,
                                ));
                            remote_tracks_for_projection = Some(tracks);
                        }
                        Err(err) => println!("- warning: unable to record remote snapshot: {err}"),
                    }
                }
                projection.recovery = if !failures.is_empty() {
                    Some(crate::state::PlaylistProjectionRecovery {
                        reason: if remote_tracks_for_projection.is_some() {
                            "projection completed partially".to_owned()
                        } else {
                            "projection may have completed partially; remote read-back was unavailable"
                                .to_owned()
                        },
                        unresolved_items: unresolved.clone(),
                        failed_appends: failures.len(),
                        next_action:
                            "refresh the linked target before retrying unresolved occurrences"
                                .to_owned(),
                    })
                } else if remote_tracks_for_projection.is_none() {
                    Some(crate::state::PlaylistProjectionRecovery {
                        reason: "remote read-back was unavailable after projection".to_owned(),
                        unresolved_items: unresolved.clone(),
                        failed_appends: 0,
                        next_action: "refresh the linked target before retrying".to_owned(),
                    })
                } else {
                    None
                };
                projection.status =
                    if let Some(remote_tracks) = remote_tracks_for_projection.as_deref() {
                        let remote_items = remote_tracks
                            .iter()
                            .map(crate::state::UnifiedPlaylistItem::from_youtube_track)
                            .collect::<Vec<_>>();
                        let plan = crate::state::dry_run_projection(
                            &playlist.items,
                            &remote_items,
                            &projection.mappings,
                        );
                        projection.conflicts = plan.conflicts;
                        if failures.is_empty() {
                            plan.status
                        } else {
                            crate::state::PlaylistProjectionStatus::Partial
                        }
                    } else {
                        crate::state::PlaylistProjectionStatus::OutcomeUnknown
                    };
                link.upsert_projection(projection);
                link.updated_at = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .context("read system clock")?
                    .as_secs();
                data.upsert_playlist_link(link)?;
                println!(
                    "Applied with {} failed item(s); existing target items were not removed",
                    failures.len()
                );
            } else {
                println!("No changes were written; pass --apply to append items");
            }
        }
        _ => unreachable!(),
    }
    Ok(())
}

#[derive(Debug, Default, PartialEq, Eq)]
struct ProjectionResolution {
    /// `YouTube` IDs in the local playlist order that are safe to append.
    desired_youtube_ids: Vec<String>,
    /// Local items that could not be projected without guessing.
    unresolved: Vec<String>,
    /// Human-readable successful matches for the dry-run report.
    matched: Vec<String>,
    /// Explicit cross-provider matches accepted by `--resolution match`.
    accepted_mappings: Vec<crate::state::PlaylistProjectionMapping>,
}

/// Resolve the local projection without mutating the local playlist.
///
/// The resolver is injected so the policy can be tested without credentials or
/// a network connection. Normal/local/merge modes never call it; only the
/// explicit `match` mode performs metadata matching for Spotify-origin items.
fn resolve_projection_items<F>(
    playlist: &crate::state::UnifiedPlaylist,
    resolution: Option<&str>,
    mut search: F,
) -> Result<ProjectionResolution>
where
    F: FnMut(&crate::state::UnifiedPlaylistItem) -> Result<Vec<crate::state::YouTubeTrack>>,
{
    let mut projection = ProjectionResolution::default();
    for item in &playlist.items {
        if item.media_id.provider == crate::state::Provider::YouTubeMusic {
            projection
                .desired_youtube_ids
                .push(item.media_id.raw_id.clone());
            continue;
        }

        if resolution == Some("match") {
            let candidates =
                search(item).with_context(|| format!("search YouTube for '{}'", item.title))?;
            if let Some((candidate, score)) = crate::state::best_youtube_match(item, &candidates) {
                projection.desired_youtube_ids.push(candidate.id.clone());
                projection.matched.push(format!(
                    "{} -> {} ({}) [{score}/105]",
                    item.title, candidate.name, candidate.id
                ));
                projection
                    .accepted_mappings
                    .push(crate::state::PlaylistProjectionMapping {
                        local_entry_id: item.entry_id,
                        remote_media_id: crate::state::MediaId {
                            provider: crate::state::Provider::YouTubeMusic,
                            kind: if candidate.is_video {
                                crate::state::MediaKind::Video
                            } else {
                                crate::state::MediaKind::Track
                            },
                            raw_id: candidate.id.clone(),
                        },
                        remote_occurrence_token: None,
                        accepted_at: 0,
                    });
                continue;
            }
        }

        projection.unresolved.push(if resolution == Some("match") {
            format!(
                "{:?}:{} ({})",
                item.media_id.provider, item.media_id.raw_id, item.title
            )
        } else {
            format!("{:?}:{}", item.media_id.provider, item.media_id.raw_id)
        });
    }
    Ok(projection)
}

fn ensure_projection_resolution_complete(
    resolution: Option<&str>,
    apply: bool,
    unresolved: &[String],
) -> Result<()> {
    anyhow::ensure!(
        !(apply && resolution == Some("match") && !unresolved.is_empty()),
        "metadata matching left unresolved item(s); no changes were written"
    );
    Ok(())
}

fn missing_youtube_ids(desired: &[String], existing: &[String]) -> Vec<String> {
    let mut available = HashMap::<&str, usize>::new();
    for id in existing {
        *available.entry(id.as_str()).or_default() += 1;
    }
    desired
        .iter()
        .filter(|id| match available.get_mut(id.as_str()) {
            Some(count) if *count > 0 => {
                *count -= 1;
                false
            }
            _ => true,
        })
        .cloned()
        .collect()
}

fn handle_listenbrainz_subcommand(args: &ArgMatches, configs: &config::Configs) -> Result<()> {
    // Restore, diff, plan, and probing remain explicit CLI-only tools. The
    // backup transport is shared with the opt-in TUI action so both surfaces
    // preserve the same partial-outcome and local-link contract.
    let (subcommand, subcommand_args) = args
        .subcommand()
        .context("listenbrainz subcommand is required")?;
    match subcommand {
        "probe" => return super::listenbrainz_probe::run(subcommand_args, configs),
        "auth" => {
            println!("ListenBrainz user token setup");
            println!("1. Open https://listenbrainz.org/settings/");
            println!(
                "2. Copy the User Token into {}",
                configs.listenbrainz_token_path().display()
            );
            println!("3. Or set LISTENBRAINZ_TOKEN for a one-session override.");
        }
        "status" => {
            println!(
                "ListenBrainz token: {}",
                if configs.listenbrainz_token().is_some() {
                    "configured"
                } else {
                    "missing"
                }
            );
            println!(
                "Token path: {}",
                configs.listenbrainz_token_path().display()
            );
        }
        "backup" => {
            let playlist_id = subcommand_args
                .get_one::<String>("playlist_id")
                .expect("playlist-id is required");
            let token = configs
                .listenbrainz_token()
                .context("ListenBrainz token is missing; run `listenbrainz auth`")?;
            let data = crate::state::AppData::new(&configs.config_folder, &configs.cache_folder);
            let playlist = data
                .unified_playlists
                .iter()
                .find(|playlist| playlist.id == *playlist_id)
                .context("unified playlist not found")?
                .clone();
            let description = super::listenbrainz_manifest::description_envelope_with_budget(
                &playlist,
                super::listenbrainz_manifest::DESCRIPTION_CHARACTER_BUDGET,
            )?;
            let remote_id = tokio::runtime::Runtime::new()?.block_on(
                crate::client::listenbrainz::create_description_backup(
                    &reqwest::Client::new(),
                    &token,
                    &playlist.name,
                    &description,
                ),
            )?;
            let mut data =
                crate::state::AppData::new(&configs.config_folder, &configs.cache_folder);
            let mut link = data
                .playlist_links
                .iter()
                .find(|link| link.unified_playlist_id == playlist.id)
                .cloned()
                .unwrap_or_else(|| crate::state::PlaylistLink {
                    unified_playlist_id: playlist.id.clone(),
                    ..crate::state::PlaylistLink::default()
                });
            link.listenbrainz_playlist_id = Some(remote_id.clone());
            link.updated_at = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .context("read system clock")?
                .as_secs();
            data.upsert_playlist_link(link).with_context(|| {
                format!(
                    "ListenBrainz backup {remote_id} succeeded, but its local link could not be saved"
                )
            })?;
            println!("{remote_id}");
        }
        "restore" => {
            let playlist_id = subcommand_args
                .get_one::<String>("playlist_id")
                .expect("playlist-id is required");
            let token = configs
                .listenbrainz_token()
                .context("ListenBrainz token is missing; run `listenbrainz auth`")?;
            let (name, items, unresolved) = fetch_listenbrainz_playlist(&token, playlist_id)?;
            let unified_id = subcommand_args
                .get_one::<String>("unified_id")
                .cloned()
                .unwrap_or_else(|| format!("listenbrainz-{playlist_id}"));
            println!(
                "ListenBrainz playlist '{name}': {} item(s), {unresolved} unresolved",
                items.len()
            );
            if subcommand_args.get_flag("apply") {
                let mut data =
                    crate::state::AppData::new(&configs.config_folder, &configs.cache_folder);
                let updated_at = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .context("read system clock")?
                    .as_secs();
                let playlist = crate::state::UnifiedPlaylist {
                    id: unified_id.clone(),
                    name,
                    items,
                    updated_at,
                    next_entry_id: 1,
                };
                let mut link = data
                    .playlist_links
                    .iter()
                    .find(|link| link.unified_playlist_id == unified_id)
                    .cloned()
                    .unwrap_or_else(|| crate::state::PlaylistLink {
                        unified_playlist_id: unified_id.clone(),
                        ..crate::state::PlaylistLink::default()
                    });
                link.listenbrainz_playlist_id = Some(playlist_id.clone());
                link.updated_at = updated_at;
                data.upsert_unified_playlist_with_link(playlist, link)?;
                println!("Restored locally as {unified_id}");
            } else {
                println!("Dry run only; pass --apply to write the local playlist");
            }
        }
        "diff" => {
            let playlist_id = subcommand_args
                .get_one::<String>("playlist_id")
                .expect("playlist-id is required");
            let unified_id = subcommand_args
                .get_one::<String>("unified_id")
                .expect("unified-id is required");
            let token = configs
                .listenbrainz_token()
                .context("ListenBrainz token is missing; run `listenbrainz auth`")?;
            let (_, remote_items, unresolved) = fetch_listenbrainz_playlist(&token, playlist_id)?;
            let data = crate::state::AppData::new(&configs.config_folder, &configs.cache_folder);
            let local = data
                .unified_playlists
                .iter()
                .find(|playlist| playlist.id == *unified_id)
                .context("unified playlist not found")?;
            let remote_ids = remote_items
                .iter()
                .map(|item| item.media_id.clone())
                .collect::<Vec<_>>();
            let local_ids = local
                .items
                .iter()
                .map(|item| item.media_id.clone())
                .collect::<Vec<_>>();
            let added = remote_ids
                .iter()
                .filter(|id| !local_ids.contains(id))
                .count();
            let removed = local_ids
                .iter()
                .filter(|id| !remote_ids.contains(id))
                .count();
            let order_differs = remote_ids != local_ids;
            println!(
                "ListenBrainz diff: {added} added, {removed} removed, order {}{}",
                if order_differs { "differs" } else { "matches" },
                if unresolved > 0 {
                    format!(", {unresolved} unresolved")
                } else {
                    String::new()
                }
            );
        }
        "plan" => {
            let playlist_id = subcommand_args
                .get_one::<String>("playlist_id")
                .expect("playlist-id is required");
            let unified_id = subcommand_args
                .get_one::<String>("unified_id")
                .expect("unified-id is required");
            let format = subcommand_args
                .get_one::<String>("format")
                .map_or("text", String::as_str);
            let expected_remote_fingerprint = subcommand_args
                .get_one::<String>("expected_remote_fingerprint")
                .map(String::as_str);
            let token = configs
                .listenbrainz_token()
                .context("ListenBrainz token is missing; run `listenbrainz auth`")?;
            let remote = fetch_listenbrainz_playlist_value(&token, playlist_id)?;
            let mut data =
                crate::state::AppData::new(&configs.config_folder, &configs.cache_folder);
            let local = data
                .unified_playlists
                .iter()
                .find(|playlist| playlist.id == *unified_id)
                .context("unified playlist not found")?
                .clone();
            let plan = crate::client::listenbrainz_sync::build_remote_anchored_plan(
                playlist_id,
                &local,
                &remote,
                expected_remote_fingerprint,
            );
            let plan_is_ready = plan.status == crate::client::listenbrainz_sync::PlanStatus::Ready;
            if subcommand_args.get_flag("initialize_base") {
                anyhow::ensure!(
                    plan_is_ready && plan.conflicts.is_empty(),
                    "cannot initialize a ListenBrainz base from an unsafe plan"
                );
                let verified_at = current_unix_timestamp()?;
                let base = crate::client::listenbrainz_push::verified_base_from_readback(
                    playlist_id,
                    &local,
                    &remote,
                    expected_remote_fingerprint,
                    verified_at,
                )?;
                data.store_verified_listenbrainz_base(
                    unified_id,
                    playlist_id,
                    crate::state::ListenBrainzSyncState::verified(base)?,
                )?;
            }
            if format == "json" {
                println!("{}", serde_json::to_string_pretty(&plan)?);
            } else {
                print_listenbrainz_sync_plan(&plan);
            }
            anyhow::ensure!(
                plan_is_ready,
                "ListenBrainz sync plan cannot be produced safely"
            );
        }
        "projection-preview" => {
            let unified_id = subcommand_args
                .get_one::<String>("unified_id")
                .expect("unified-id is required");
            let format = subcommand_args
                .get_one::<String>("format")
                .map_or("text", String::as_str);
            let annotation_budget = *subcommand_args
                .get_one::<usize>("description_budget")
                .expect("description-budget has a default");
            let data = crate::state::AppData::new(&configs.config_folder, &configs.cache_folder);
            let playlist = data
                .unified_playlists
                .iter()
                .find(|playlist| playlist.id == *unified_id)
                .context("unified playlist not found")?;
            let relationships = parse_recording_relations(
                subcommand_args
                    .get_many::<String>("recording_relation")
                    .into_iter()
                    .flatten()
                    .map(String::as_str),
            )?;
            let (_, report) = crate::client::listenbrainz_projection::preview_native_projection(
                playlist,
                &relationships,
                annotation_budget,
            )?;
            let ready = report.is_ready();
            if format == "json" {
                println!("{}", serde_json::to_string_pretty(&report)?);
            } else {
                print_listenbrainz_projection_preview(&report);
            }
            anyhow::ensure!(ready, "ListenBrainz projection exceeds the manifest budget");
        }
        "push" => {
            let playlist_id = subcommand_args
                .get_one::<String>("playlist_id")
                .expect("playlist-id is required");
            let unified_id = subcommand_args
                .get_one::<String>("unified_id")
                .expect("unified-id is required");
            let format = subcommand_args
                .get_one::<String>("format")
                .map_or("text", String::as_str);
            let apply = subcommand_args.get_flag("apply");
            let relationships = parse_recording_relations(
                subcommand_args
                    .get_many::<String>("recording_relation")
                    .into_iter()
                    .flatten()
                    .map(String::as_str),
            )?;
            let mut data =
                crate::state::AppData::new(&configs.config_folder, &configs.cache_folder);
            let local = data
                .unified_playlists
                .iter()
                .find(|playlist| playlist.id == *unified_id)
                .context("unified playlist not found")?
                .clone();
            let (projection, preview) =
                crate::client::listenbrainz_projection::preview_native_projection(
                    &local,
                    &relationships,
                    super::listenbrainz_manifest::DESCRIPTION_CHARACTER_BUDGET,
                )?;
            anyhow::ensure!(
                preview.is_ready(),
                "ListenBrainz projection exceeds the manifest budget"
            );
            if !apply {
                if format == "json" {
                    println!("{}", serde_json::to_string_pretty(&preview)?);
                } else {
                    print_listenbrainz_projection_preview(&preview);
                    println!(
                        "Dry run only; pass --apply and --operation-id to write ListenBrainz."
                    );
                }
                return Ok(());
            }
            let operation_id = subcommand_args
                .get_one::<String>("operation_id")
                .filter(|value| !value.trim().is_empty())
                .context("--operation-id is required with --apply")?;
            let token = configs
                .listenbrainz_token()
                .context("ListenBrainz token is missing; run `listenbrainz auth`")?;
            let adapter = crate::client::listenbrainz_push::ListenBrainzMutationAdapter::new(
                reqwest::Client::new(),
            );
            let result = tokio::runtime::Runtime::new()?.block_on(
                crate::client::listenbrainz_push::execute_push_transaction(
                    &adapter,
                    &mut data,
                    &token,
                    playlist_id,
                    &local,
                    &projection,
                    operation_id,
                    current_unix_timestamp()?,
                ),
            )?;
            print_listenbrainz_push_result(playlist_id, &preview, &result, format)?;
            anyhow::ensure!(
                result.status == crate::client::listenbrainz_push::PushTransactionStatus::Verified,
                "ListenBrainz push did not reach verified state"
            );
        }
        "recover" => {
            let playlist_id = subcommand_args
                .get_one::<String>("playlist_id")
                .expect("playlist-id is required");
            let unified_id = subcommand_args
                .get_one::<String>("unified_id")
                .expect("unified-id is required");
            let format = subcommand_args
                .get_one::<String>("format")
                .map_or("text", String::as_str);
            let token = configs
                .listenbrainz_token()
                .context("ListenBrainz token is missing; run `listenbrainz auth`")?;
            let mut data =
                crate::state::AppData::new(&configs.config_folder, &configs.cache_folder);
            let local = data
                .unified_playlists
                .iter()
                .find(|playlist| playlist.id == *unified_id)
                .context("unified playlist not found")?
                .clone();
            let observed_at = current_unix_timestamp()?;
            let adapter = crate::client::listenbrainz_push::ListenBrainzMutationAdapter::new(
                reqwest::Client::new(),
            );
            let remote = tokio::runtime::Runtime::new()?
                .block_on(adapter.fetch(&token, playlist_id))
                .ok();
            let verified = remote.as_ref().and_then(|remote| {
                crate::client::listenbrainz_push::verified_base_from_readback(
                    playlist_id,
                    &local,
                    remote,
                    None,
                    observed_at,
                )
                .ok()
            });
            let disposition = data.recover_listenbrainz_sync_intent(
                unified_id,
                playlist_id,
                verified,
                observed_at,
            )?;
            print_listenbrainz_recovery_result(playlist_id, disposition, format)?;
            anyhow::ensure!(
                matches!(
                    disposition,
                    crate::state::ListenBrainzRecoveryDisposition::AlreadyApplied
                        | crate::state::ListenBrainzRecoveryDisposition::NoPendingIntent
                ),
                "ListenBrainz pending outcome remains unresolved"
            );
        }
        "pull-preview" => {
            let playlist_id = subcommand_args
                .get_one::<String>("playlist_id")
                .expect("playlist-id is required");
            let unified_id = subcommand_args
                .get_one::<String>("unified_id")
                .expect("unified-id is required");
            let format = subcommand_args
                .get_one::<String>("format")
                .map_or("text", String::as_str);
            let token = configs
                .listenbrainz_token()
                .context("ListenBrainz token is missing; run `listenbrainz auth`")?;
            let remote = fetch_listenbrainz_playlist_value(&token, playlist_id)?;
            let data = crate::state::AppData::new(&configs.config_folder, &configs.cache_folder);
            let local = data
                .unified_playlists
                .iter()
                .find(|playlist| playlist.id == *unified_id)
                .context("unified playlist not found")?;
            let link = data
                .playlist_links
                .iter()
                .find(|link| link.unified_playlist_id == *unified_id)
                .context("unified playlist link not found")?;
            anyhow::ensure!(
                link.listenbrainz_playlist_id.as_deref() == Some(playlist_id),
                "ListenBrainz link targets a different remote playlist"
            );
            let base = &link
                .listenbrainz_sync
                .as_ref()
                .context("ListenBrainz sync base is not initialized")?
                .base;
            let preview = crate::client::listenbrainz_sync::build_persisted_base_pull_preview(
                playlist_id,
                base,
                local,
                &remote,
            );
            if format == "json" {
                println!("{}", serde_json::to_string_pretty(&preview)?);
            } else {
                print_listenbrainz_pull_preview(&preview);
            }
            anyhow::ensure!(
                preview.plan.status == crate::client::listenbrainz_sync::PlanStatus::Ready,
                "ListenBrainz pull preview cannot be produced safely"
            );
        }
        "pull" => {
            let playlist_id = subcommand_args
                .get_one::<String>("playlist_id")
                .expect("playlist-id is required");
            let unified_id = subcommand_args
                .get_one::<String>("unified_id")
                .expect("unified-id is required");
            let format = subcommand_args
                .get_one::<String>("format")
                .map_or("text", String::as_str);
            let apply = subcommand_args.get_flag("apply");
            let token = configs
                .listenbrainz_token()
                .context("ListenBrainz token is missing; run `listenbrainz auth`")?;
            let remote = fetch_listenbrainz_playlist_value(&token, playlist_id)?;
            let mut data =
                crate::state::AppData::new(&configs.config_folder, &configs.cache_folder);
            let local = data
                .unified_playlists
                .iter()
                .find(|playlist| playlist.id == *unified_id)
                .context("unified playlist not found")?
                .clone();
            let link = data
                .playlist_links
                .iter()
                .find(|link| link.unified_playlist_id == *unified_id)
                .context("unified playlist link not found")?;
            anyhow::ensure!(
                link.listenbrainz_playlist_id.as_deref() == Some(playlist_id),
                "ListenBrainz link targets a different remote playlist"
            );
            let base = &link
                .listenbrainz_sync
                .as_ref()
                .context("ListenBrainz sync base is not initialized")?
                .base;
            let preview = crate::client::listenbrainz_sync::build_persisted_base_pull_preview(
                playlist_id,
                base,
                &local,
                &remote,
            );
            if !apply {
                if format == "json" {
                    println!("{}", serde_json::to_string_pretty(&preview)?);
                } else {
                    print_listenbrainz_pull_preview(&preview);
                    println!(
                        "Dry run only; pass --apply and --operation-id to update local state."
                    );
                }
                return Ok(());
            }
            let operation_id = subcommand_args
                .get_one::<String>("operation_id")
                .filter(|value| !value.trim().is_empty())
                .context("--operation-id is required with --apply")?;
            let (preview, result) = crate::client::listenbrainz_pull::execute_pull_apply(
                &mut data,
                playlist_id,
                unified_id,
                &remote,
                operation_id,
                current_unix_timestamp()?,
            )?;
            print_listenbrainz_pull_apply_result(playlist_id, &preview, &result, format)?;
        }
        "pull-rollback" => {
            let playlist_id = subcommand_args
                .get_one::<String>("playlist_id")
                .expect("playlist-id is required");
            let unified_id = subcommand_args
                .get_one::<String>("unified_id")
                .expect("unified-id is required");
            let format = subcommand_args
                .get_one::<String>("format")
                .map_or("text", String::as_str);
            let mut data =
                crate::state::AppData::new(&configs.config_folder, &configs.cache_folder);
            let operation_id = data.rollback_listenbrainz_pull_apply(unified_id, playlist_id)?;
            if format == "json" {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&serde_json::json!({
                        "remote_playlist_mbid": playlist_id,
                        "rolled_back": true,
                        "operation_id": operation_id,
                        "remote_writes_performed": false,
                        "local_writes_performed": true,
                    }))?
                );
            } else {
                println!("ListenBrainz local pull rollback complete");
                println!("Remote playlist MBID: {playlist_id}");
                println!("Operation ID: {operation_id}");
                println!("Remote writes: none; local writes: completed");
            }
        }
        "resolve" => {
            let playlist_id = subcommand_args
                .get_one::<String>("playlist_id")
                .expect("playlist-id is required");
            let unified_id = subcommand_args
                .get_one::<String>("unified_id")
                .expect("unified-id is required");
            let format = subcommand_args
                .get_one::<String>("format")
                .map_or("text", String::as_str);
            let policy = match subcommand_args
                .get_one::<String>("policy")
                .expect("policy is required")
                .as_str()
            {
                "keep-local" => crate::client::listenbrainz_resolution::ResolutionPolicy::KeepLocal,
                "keep-listenbrainz" => {
                    crate::client::listenbrainz_resolution::ResolutionPolicy::KeepListenBrainz
                }
                "merge" => {
                    crate::client::listenbrainz_resolution::ResolutionPolicy::MergeNonConflicting
                }
                _ => unreachable!("clap validates resolution policies"),
            };
            let decisions = parse_listenbrainz_conflict_decisions(
                subcommand_args
                    .get_many::<String>("decision")
                    .into_iter()
                    .flatten()
                    .map(String::as_str),
            )?;
            let unlinked_imports = parse_listenbrainz_unlinked_imports(
                subcommand_args
                    .get_many::<String>("import_unlinked")
                    .into_iter()
                    .flatten()
                    .map(String::as_str),
            )?;
            let token = configs
                .listenbrainz_token()
                .context("ListenBrainz token is missing; run `listenbrainz auth`")?;
            let remote = fetch_listenbrainz_playlist_value(&token, playlist_id)?;
            let mut data =
                crate::state::AppData::new(&configs.config_folder, &configs.cache_folder);
            let local = data
                .unified_playlists
                .iter()
                .find(|playlist| playlist.id == *unified_id)
                .context("unified playlist not found")?
                .clone();
            let base = data
                .playlist_links
                .iter()
                .find(|link| link.unified_playlist_id == *unified_id)
                .and_then(|link| link.listenbrainz_sync.as_ref())
                .context("ListenBrainz sync base is not initialized")?
                .base
                .clone();
            let observed_at = current_unix_timestamp()?;
            let plan = crate::client::listenbrainz_resolution::build_resolution_plan(
                playlist_id,
                &base,
                &local,
                &remote,
                policy,
                &decisions,
                &unlinked_imports,
                observed_at,
            )?;
            if !subcommand_args.get_flag("apply") {
                print_listenbrainz_resolution_preview(playlist_id, &plan, format)?;
                return Ok(());
            }
            anyhow::ensure!(plan.preview.ready, "ListenBrainz resolution is incomplete");
            let operation_id = subcommand_args
                .get_one::<String>("operation_id")
                .filter(|value| !value.trim().is_empty())
                .context("--operation-id is required with --apply")?;
            let relationships = parse_recording_relations(
                subcommand_args
                    .get_many::<String>("recording_relation")
                    .into_iter()
                    .flatten()
                    .map(String::as_str),
            )?;
            let adapter = crate::client::listenbrainz_push::ListenBrainzMutationAdapter::new(
                reqwest::Client::new(),
            );
            let (plan, projection, result) = tokio::runtime::Runtime::new()?.block_on(
                crate::client::listenbrainz_resolution::execute_resolution(
                    &adapter,
                    &mut data,
                    &token,
                    playlist_id,
                    unified_id,
                    &remote,
                    policy,
                    &decisions,
                    &unlinked_imports,
                    &relationships,
                    operation_id,
                    observed_at,
                ),
            )?;
            print_listenbrainz_resolution_result(playlist_id, &plan, &projection, &result, format)?;
            anyhow::ensure!(result.completed, "ListenBrainz resolution did not complete");
        }
        _ => unreachable!(),
    }
    Ok(())
}

fn parse_recording_relations<'a>(
    values: impl IntoIterator<Item = &'a str>,
) -> Result<Vec<crate::client::listenbrainz_projection::ExplicitRecordingRelation>> {
    values
        .into_iter()
        .map(|value| {
            let (media, recording_mbid) = value
                .split_once('=')
                .context("recording relation must use PROVIDER:KIND:ID=MBID")?;
            let mut parts = media.splitn(3, ':');
            let provider = parts
                .next()
                .and_then(super::listenbrainz_manifest::parse_provider)
                .context("recording relation has an unsupported provider")?;
            let kind = parts
                .next()
                .and_then(super::listenbrainz_manifest::parse_media_kind)
                .context("recording relation has an unsupported media kind")?;
            let raw_id = parts
                .next()
                .filter(|value| !value.trim().is_empty())
                .context("recording relation has no provider media ID")?;
            Ok(
                crate::client::listenbrainz_projection::ExplicitRecordingRelation {
                    media_id: crate::state::MediaId {
                        provider,
                        kind,
                        raw_id: raw_id.to_owned(),
                    },
                    recording_mbid: recording_mbid.to_owned(),
                },
            )
        })
        .collect()
}

fn parse_listenbrainz_conflict_decisions<'a>(
    values: impl IntoIterator<Item = &'a str>,
) -> Result<Vec<crate::client::listenbrainz_resolution::ConflictDecision>> {
    values
        .into_iter()
        .map(|value| {
            let (index, side) = value
                .split_once('=')
                .context("conflict decision must use INDEX=local|listenbrainz")?;
            let conflict_index = index
                .parse::<usize>()
                .context("conflict decision index must be a non-negative integer")?;
            let side = match side {
                "local" => crate::client::listenbrainz_resolution::ResolutionSide::Local,
                "listenbrainz" => {
                    crate::client::listenbrainz_resolution::ResolutionSide::ListenBrainz
                }
                _ => anyhow::bail!("conflict decision side must be local or listenbrainz"),
            };
            Ok(crate::client::listenbrainz_resolution::ConflictDecision {
                conflict_index,
                side,
            })
        })
        .collect()
}

fn parse_listenbrainz_unlinked_imports<'a>(
    values: impl IntoIterator<Item = &'a str>,
) -> Result<Vec<crate::client::listenbrainz_resolution::UnlinkedImport>> {
    values
        .into_iter()
        .map(|value| {
            let mut fields = value.splitn(3, '=');
            let conflict_index = fields
                .next()
                .context("unlinked import has no conflict index")?
                .parse::<usize>()
                .context("unlinked import index must be a non-negative integer")?;
            let media = fields
                .next()
                .context("unlinked import has no provider media identity")?;
            let recording_mbid = fields
                .next()
                .context("unlinked import has no recording MBID")?;
            let mut parts = media.splitn(3, ':');
            let provider = parts
                .next()
                .and_then(super::listenbrainz_manifest::parse_provider)
                .context("unlinked import has an unsupported provider")?;
            let kind = parts
                .next()
                .and_then(super::listenbrainz_manifest::parse_media_kind)
                .context("unlinked import has an unsupported media kind")?;
            let raw_id = parts
                .next()
                .filter(|value| !value.trim().is_empty())
                .context("unlinked import has no provider media ID")?;
            Ok(crate::client::listenbrainz_resolution::UnlinkedImport {
                conflict_index,
                media_id: crate::state::MediaId {
                    provider,
                    kind,
                    raw_id: raw_id.to_owned(),
                },
                recording_mbid: recording_mbid.to_owned(),
            })
        })
        .collect()
}

fn print_listenbrainz_projection_preview(
    report: &crate::client::listenbrainz_projection::NativeProjectionPreview,
) {
    println!("ListenBrainz native projection preview");
    println!("Status: {:?}", report.status);
    println!(
        "Occurrences: {} total, {} native, {} manifest-only, {} unresolved, {} ineligible, {} duplicate native",
        report.total_occurrences,
        report.native_rows,
        report.manifest_only,
        report.unresolved,
        report.ineligible,
        report.duplicate_native_rows,
    );
    println!(
        "Annotation budget: {} / {} characters",
        report.annotation_characters, report.annotation_budget
    );
    println!("Remote writes: none");
}

fn print_listenbrainz_push_result(
    remote_playlist_id: &str,
    preview: &crate::client::listenbrainz_projection::NativeProjectionPreview,
    result: &crate::client::listenbrainz_push::PushTransactionResult,
    format: &str,
) -> Result<()> {
    if format == "json" {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "remote_playlist_mbid": remote_playlist_id,
                "preview": preview,
                "transaction": result,
                "next_action": result.next_action(),
            }))?
        );
    } else {
        print_listenbrainz_projection_preview(preview);
        println!("Remote playlist MBID: {remote_playlist_id}");
        println!("Push status: {:?}", result.status);
        if let Some(stage) = result.stage {
            println!("Stage: {stage:?}");
        }
        println!("Next action: {}", result.next_action());
    }
    Ok(())
}

fn print_listenbrainz_recovery_result(
    remote_playlist_id: &str,
    disposition: crate::state::ListenBrainzRecoveryDisposition,
    format: &str,
) -> Result<()> {
    let next_action = match disposition {
        crate::state::ListenBrainzRecoveryDisposition::AlreadyApplied
        | crate::state::ListenBrainzRecoveryDisposition::NoPendingIntent => "none",
        crate::state::ListenBrainzRecoveryDisposition::Conflict => {
            "review the remote conflict before choosing a resolution"
        }
        crate::state::ListenBrainzRecoveryDisposition::OutcomeUnknown => {
            "inspect the remote playlist; do not retry the push"
        }
        crate::state::ListenBrainzRecoveryDisposition::Rejected
        | crate::state::ListenBrainzRecoveryDisposition::Partial
        | crate::state::ListenBrainzRecoveryDisposition::VerificationFailed => {
            "run listenbrainz recover again only after the remote state can be read"
        }
    };
    if format == "json" {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "remote_playlist_mbid": remote_playlist_id,
                "disposition": disposition,
                "next_action": next_action,
                "writes_performed": false,
            }))?
        );
    } else {
        println!("Remote playlist MBID: {remote_playlist_id}");
        println!("Recovery status: {disposition:?}");
        println!("Next action: {next_action}");
        println!("Remote writes: none");
    }
    Ok(())
}

fn print_listenbrainz_pull_preview(
    preview: &crate::client::listenbrainz_sync::ListenBrainzPullPreview,
) {
    println!("ListenBrainz pull preview (read only)");
    println!("Classification: {:?}", preview.classification);
    println!(
        "Occurrences: {} base, {} local, {} remote; {} native, {} unresolved",
        preview.base_occurrences,
        preview.local_occurrences,
        preview.remote_occurrences,
        preview.remote_native_rows,
        preview.remote_unresolved,
    );
    println!(
        "Affected occurrences: {}",
        preview.affected_occurrences.len()
    );
    println!("Conflicts: {}", preview.plan.conflicts.len());
    println!("Remote writes: none; local writes: none");
}

fn print_listenbrainz_pull_apply_result(
    remote_playlist_id: &str,
    preview: &crate::client::listenbrainz_sync::ListenBrainzPullPreview,
    result: &crate::client::listenbrainz_pull::PullApplyResult,
    format: &str,
) -> Result<()> {
    if format == "json" {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "remote_playlist_mbid": remote_playlist_id,
                "preview": preview,
                "result": result,
                "next_action": "none",
            }))?
        );
    } else {
        print_listenbrainz_pull_preview(preview);
        println!("Remote playlist MBID: {remote_playlist_id}");
        println!(
            "Local apply: completed; {} occurrences, {} unresolved",
            result.occurrences, result.unresolved
        );
        println!("Rollback available: {}", result.rollback_available);
        println!("Remote writes: none; local writes: completed");
    }
    Ok(())
}

fn print_listenbrainz_resolution_preview(
    remote_playlist_id: &str,
    plan: &crate::client::listenbrainz_resolution::ResolutionPlan,
    format: &str,
) -> Result<()> {
    if format == "json" {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "remote_playlist_mbid": remote_playlist_id,
                "resolution": plan.preview,
                "conflicts": plan.pull.plan.conflicts,
            }))?
        );
    } else {
        println!("ListenBrainz conflict resolution preview");
        println!("Policy: {:?}", plan.preview.policy);
        println!("Ready: {}", plan.preview.ready);
        println!(
            "Consequences: {} add, {} remove, reorder={}, rename={}",
            plan.preview.additions,
            plan.preview.removals,
            plan.preview.reorder,
            plan.preview.rename
        );
        println!(
            "Affected occurrences: {}",
            plan.preview.affected_occurrences.len()
        );
        println!(
            "Conflict decisions: {} supplied / {} required",
            plan.preview.decisions_supplied, plan.preview.decisions_required
        );
        for (index, conflict) in plan.pull.plan.conflicts.iter().enumerate() {
            println!(
                "Conflict {index}: {:?}, occurrence={}",
                conflict.kind,
                conflict
                    .occurrence
                    .map_or_else(|| "none".to_owned(), |value| value.to_string())
            );
        }
        println!(
            "Writes required: local={}, remote={}",
            plan.preview.local_write_required, plan.preview.remote_write_required
        );
        println!("Dry run only; pass --apply and --operation-id to continue.");
    }
    Ok(())
}

fn print_listenbrainz_resolution_result(
    remote_playlist_id: &str,
    plan: &crate::client::listenbrainz_resolution::ResolutionPlan,
    projection: &crate::client::listenbrainz_projection::NativeProjectionPreview,
    result: &crate::client::listenbrainz_resolution::ResolutionApplyResult,
    format: &str,
) -> Result<()> {
    if format == "json" {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "remote_playlist_mbid": remote_playlist_id,
                "resolution": plan.preview,
                "projection": projection,
                "result": result,
            }))?
        );
    } else {
        println!("ListenBrainz conflict resolution result");
        println!("Policy: {:?}", plan.preview.policy);
        println!(
            "Consequences: {} add, {} remove, reorder={}, rename={}",
            plan.preview.additions,
            plan.preview.removals,
            plan.preview.reorder,
            plan.preview.rename
        );
        println!("Resolution completed: {}", result.completed);
        println!("Remote status: {:?}", result.remote_status);
        println!("Local applied: {}", result.local_applied);
        println!("Rollback available: {}", result.rollback_available);
    }
    Ok(())
}

fn current_unix_timestamp() -> Result<u64> {
    Ok(std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .context("read system clock")?
        .as_secs())
}

fn fetch_listenbrainz_playlist(
    token: &str,
    playlist_id: &str,
) -> Result<(String, Vec<crate::state::UnifiedPlaylistItem>, usize)> {
    parse_listenbrainz_playlist(&fetch_listenbrainz_playlist_value(token, playlist_id)?)
}

fn fetch_listenbrainz_playlist_value(token: &str, playlist_id: &str) -> Result<serde_json::Value> {
    let (status, body): (reqwest::StatusCode, serde_json::Value) = tokio::runtime::Runtime::new()?
        .block_on(async {
            let response = reqwest::Client::new()
                .get(format!(
                    "https://api.listenbrainz.org/1/playlist/{playlist_id}"
                ))
                .header(reqwest::header::AUTHORIZATION, format!("Token {token}"))
                .send()
                .await?;
            let status = response.status();
            let body = response.json().await?;
            Ok::<_, reqwest::Error>((status, body))
        })?;
    anyhow::ensure!(
        status.is_success(),
        "ListenBrainz playlist fetch failed ({status}): {body}"
    );
    Ok(body)
}

#[cfg(feature = "private-capture")]
fn handle_youtube_debug(args: &ArgMatches, _configs: &config::Configs) -> Result<()> {
    anyhow::ensure!(
        std::io::stdout().is_terminal(),
        "YouTube developer inspection requires an interactive output terminal"
    );
    let (command, command_args) = args
        .subcommand()
        .context("YouTube debug command is required")?;
    anyhow::ensure!(
        command_args.get_flag("acknowledge_sensitive"),
        "YouTube developer inspection requires --acknowledge-sensitive"
    );
    let runtime = tokio::runtime::Runtime::new()?;
    match command {
        "inspect" => {
            let video_id = command_args
                .get_one::<String>("video_id")
                .context("video ID is required")?;
            let report = runtime.block_on(client::inspect_youtube_for_video(
                video_id,
                command_args.get_flag("transport"),
            ))?;
            render_youtube_developer_report(
                "YouTube developer inspection",
                &report,
                command_args.get_flag("json"),
            )?;
        }
        "compare" => {
            let config_a = command_args
                .get_one::<std::path::PathBuf>("config_a")
                .context("config-a is required")?;
            let cache_a = command_args
                .get_one::<std::path::PathBuf>("cache_a")
                .context("cache-a is required")?;
            let config_b = command_args
                .get_one::<std::path::PathBuf>("config_b")
                .context("config-b is required")?;
            let cache_b = command_args
                .get_one::<std::path::PathBuf>("cache_b")
                .context("cache-b is required")?;
            let video_id = command_args
                .get_one::<String>("video_id")
                .context("video ID is required")?;
            let configs_a = config::Configs::new_without_account_bootstrap(config_a, cache_a)
                .context("load account A configuration")?;
            let configs_b = config::Configs::new_without_account_bootstrap(config_b, cache_b)
                .context("load account B configuration")?;
            let include_transport = command_args.get_flag("transport");
            let report_a = runtime.block_on(client::inspect_youtube_for_video_with_configs(
                &configs_a,
                video_id,
                include_transport,
            ))?;
            let report_b = runtime.block_on(client::inspect_youtube_for_video_with_configs(
                &configs_b,
                video_id,
                include_transport,
            ))?;
            render_youtube_developer_comparison(
                video_id,
                &report_a,
                &report_b,
                command_args.get_flag("json"),
            )?;
        }
        _ => anyhow::bail!("unknown YouTube debug command"),
    }
    Ok(())
}

#[cfg(feature = "private-capture")]
fn render_youtube_developer_report(
    title: &str,
    report: &client::YouTubeDeveloperInspection,
    json: bool,
) -> Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(report)?);
        return Ok(());
    }
    println!("{title}");
    println!("sensitivity=private");
    println!("credentials=masked");
    println!("signed_urls=masked");
    println!(
        "account_id={}",
        report
            .account_id
            .as_deref()
            .map_or_else(|| "unknown".to_owned(), private_single_line)
    );
    println!(
        "account_label={}",
        report
            .account_label
            .as_deref()
            .map_or_else(|| "unknown".to_owned(), private_single_line)
    );
    println!("auth_type={}", report.auth_type);
    println!(
        "library=playlists:{} albums:{} artists:{} warnings:{} status={}",
        report.library.playlists,
        report.library.albums,
        report.library.artists,
        report.library.warning_count,
        report.library_status,
    );
    let player = &report.player;
    println!("player.video_id={}", private_single_line(&player.video_id));
    println!("player.auth={}", player.auth_kind);
    println!(
        "player.client={} kind={} version={} version_source={} sts_present={} hl={} gl={}",
        player.client.context_name,
        player.client.kind,
        private_single_line(&player.client.version),
        player.client.version_source,
        player.signature_timestamp_present,
        player.client.language,
        player.client.region
    );
    println!("player.proof_token_present={}", player.proof_token_present);
    println!("player.http_status={}", player.http_status);
    println!("player.playability_status={}", player.playability_status);
    println!(
        "player.playability_reason={}",
        player
            .playability_reason
            .as_deref()
            .map_or("none".to_owned(), private_single_line)
    );
    println!(
        "formats=adaptive:{} direct_audio:{} cipher_audio:{} streaming_data:{}",
        player.adaptive_format_count,
        player.direct_audio_count,
        player.cipher_audio_count,
        player.streaming_data_present
    );
    println!(
        "selection={} selected_audio_itag={}",
        player.selection,
        player
            .selected_audio_itag
            .map_or_else(|| "none".to_owned(), |itag| itag.to_string())
    );
    println!("client_matrix=attempts:{}", player.client_matrix.len());
    for (index, attempt) in player.client_matrix.iter().enumerate() {
        println!(
            "client_attempt[{index}].client={} kind={} version={} version_source={} sts_present={} status={} playability={} reason={} formats=adaptive:{} direct_audio:{} cipher_audio:{} streaming_data:{} selection={} selected_audio_itag={} error={}",
            attempt.client.context_name,
            attempt.client.kind,
            private_single_line(&attempt.client.version),
            attempt.client.version_source,
            attempt.signature_timestamp_present,
            attempt.http_status,
            attempt.playability_status,
            attempt
                .playability_reason
                .as_deref()
                .map_or("none".to_owned(), private_single_line),
            attempt.adaptive_format_count,
            attempt.direct_audio_count,
            attempt.cipher_audio_count,
            attempt.streaming_data_present,
            attempt.selection,
            attempt
                .selected_audio_itag
                .map_or_else(|| "none".to_owned(), |itag| itag.to_string()),
            attempt.error_category.as_deref().unwrap_or("none"),
        );
    }
    render_youtube_transport(
        report.transport.as_ref(),
        report.transport_error_category.as_deref(),
    );
    Ok(())
}

#[cfg(feature = "private-capture")]
fn render_youtube_transport(
    transport: Option<&client::MediaTransportDiagnostic>,
    error_category: Option<&str>,
) {
    if let Some(transport) = transport {
        println!("transport={transport}");
    } else if let Some(category) = error_category {
        println!("transport_error_category={category}");
    } else {
        println!("transport=not-requested");
    }
}

#[cfg(feature = "private-capture")]
fn render_youtube_developer_comparison(
    video_id: &str,
    report_a: &client::YouTubeDeveloperInspection,
    report_b: &client::YouTubeDeveloperInspection,
    json: bool,
) -> Result<()> {
    let differences = youtube_developer_differences(report_a, report_b);
    if json {
        let output = serde_json::json!({
            "sensitivity": "private",
            "video_id": video_id,
            "differences": differences,
            "account_a": report_a,
            "account_b": report_b,
        });
        println!("{}", serde_json::to_string_pretty(&output)?);
        return Ok(());
    }
    println!("YouTube developer account comparison");
    println!("sensitivity=private");
    println!("credentials=masked");
    println!("signed_urls=masked");
    println!("video_id={}", private_single_line(video_id));
    println!(
        "account_a={} ({})",
        report_a
            .account_id
            .as_deref()
            .map_or_else(|| "unknown".to_owned(), private_single_line),
        report_a
            .account_label
            .as_deref()
            .map_or_else(|| "unknown".to_owned(), private_single_line)
    );
    println!(
        "account_b={} ({})",
        report_b
            .account_id
            .as_deref()
            .map_or_else(|| "unknown".to_owned(), private_single_line),
        report_b
            .account_label
            .as_deref()
            .map_or_else(|| "unknown".to_owned(), private_single_line)
    );
    if differences.is_empty() {
        println!("differences=none");
    } else {
        println!("differences={}", differences.join(","));
    }
    render_youtube_developer_report("account_a", report_a, false)?;
    render_youtube_developer_report("account_b", report_b, false)?;
    Ok(())
}

#[cfg(feature = "private-capture")]
fn youtube_developer_differences(
    report_a: &client::YouTubeDeveloperInspection,
    report_b: &client::YouTubeDeveloperInspection,
) -> Vec<&'static str> {
    let mut differences = Vec::new();
    if report_a.auth_type != report_b.auth_type {
        differences.push("auth_type");
    }
    if report_a.library.playlists != report_b.library.playlists {
        differences.push("library.playlists");
    }
    if report_a.library.albums != report_b.library.albums {
        differences.push("library.albums");
    }
    if report_a.library.artists != report_b.library.artists {
        differences.push("library.artists");
    }
    if report_a.library.warning_count != report_b.library.warning_count {
        differences.push("library.warning_count");
    }
    if report_a.library_status != report_b.library_status {
        differences.push("library.status");
    }
    let player_a = &report_a.player;
    let player_b = &report_b.player;
    if player_a.auth_kind != player_b.auth_kind {
        differences.push("player.auth");
    }
    if player_a.client.context_name != player_b.client.context_name {
        differences.push("player.client.name");
    }
    if player_a.client.version != player_b.client.version {
        differences.push("player.client.version");
    }
    if player_a.client.kind != player_b.client.kind {
        differences.push("player.client.kind");
    }
    if player_a.client.version_source != player_b.client.version_source {
        differences.push("player.client.version_source");
    }
    if player_a.client.language != player_b.client.language {
        differences.push("player.client.language");
    }
    if player_a.client.region != player_b.client.region {
        differences.push("player.client.region");
    }
    if player_a.proof_token_present != player_b.proof_token_present {
        differences.push("player.proof_token_present");
    }
    if player_a.http_status != player_b.http_status {
        differences.push("player.http_status");
    }
    if player_a.playability_status != player_b.playability_status {
        differences.push("player.playability_status");
    }
    if player_a.playability_reason != player_b.playability_reason {
        differences.push("player.playability_reason");
    }
    if player_a.streaming_data_present != player_b.streaming_data_present {
        differences.push("player.streaming_data_present");
    }
    if player_a.adaptive_format_count != player_b.adaptive_format_count {
        differences.push("player.adaptive_format_count");
    }
    if player_a.direct_audio_count != player_b.direct_audio_count {
        differences.push("player.direct_audio_count");
    }
    if player_a.cipher_audio_count != player_b.cipher_audio_count {
        differences.push("player.cipher_audio_count");
    }
    if player_a.selected_audio_itag != player_b.selected_audio_itag {
        differences.push("player.selected_audio_itag");
    }
    if player_a.selection != player_b.selection {
        differences.push("player.selection");
    }
    if serde_json::to_value(&player_a.client_matrix).ok()
        != serde_json::to_value(&player_b.client_matrix).ok()
    {
        differences.push("player.client_matrix");
    }
    if serde_json::to_value(&report_a.transport).ok()
        != serde_json::to_value(&report_b.transport).ok()
        || report_a.transport_error_category != report_b.transport_error_category
    {
        differences.push("transport");
    }
    differences
}

#[cfg(feature = "private-capture")]
fn private_single_line(value: &str) -> String {
    value
        .chars()
        .map(|character| {
            if character.is_control() {
                ' '
            } else {
                character
            }
        })
        .take(512)
        .collect::<String>()
}

fn print_listenbrainz_sync_plan(plan: &crate::client::listenbrainz_sync::ListenBrainzSyncPlan) {
    println!(
        "ListenBrainz three-way plan (read only): {} -> {}",
        plan.remote_playlist_id, plan.unified_playlist_id
    );
    println!("Status: {:?}", plan.status);
    if let Some(reason) = plan.cannot_plan_reason {
        println!("Cannot plan: {reason:?}");
    }
    if let Some(fingerprint) = &plan.remote_fingerprint {
        println!("Remote fingerprint: {fingerprint}");
    }
    println!("Changes: {}", plan.changes.len());
    for change in &plan.changes {
        let occurrence = change
            .occurrence
            .map_or_else(String::new, |value| format!(" occurrence={value}"));
        println!(
            "- [{:?}/{:?}]{occurrence}",
            change.classification, change.kind
        );
    }
    println!("Conflicts: {}", plan.conflicts.len());
    for conflict in &plan.conflicts {
        println!("- [{:?}] {}", conflict.kind, conflict.message);
    }
    for warning in &plan.warnings {
        println!("Warning [{:?}]: {}", warning.kind, warning.message);
    }
    println!("No changes were written.");
}

#[cfg(test)]
mod tests {
    use super::{
        diagnostic_response_text, ensure_projection_resolution_complete, missing_youtube_ids,
        parse_listenbrainz_playlist, parse_recording_relations, resolve_projection_items,
        unified_remove_entry_ids, ProjectionResolution, Response,
    };
    use crate::cli::listenbrainz_manifest::description_envelope;

    fn projection_item(
        provider: crate::state::Provider,
        raw_id: &str,
        title: &str,
        artists: &str,
    ) -> crate::state::UnifiedPlaylistItem {
        crate::state::UnifiedPlaylistItem {
            media_id: crate::state::MediaId {
                provider,
                kind: crate::state::MediaKind::Track,
                raw_id: raw_id.to_owned(),
            },
            title: title.to_owned(),
            artists: artists.to_owned(),
            duration_ms: Some(229_000),
            provider_url: None,
            ..crate::state::UnifiedPlaylistItem::default()
        }
    }

    #[test]
    fn recording_relation_parser_accepts_only_explicit_typed_media_identity() {
        let relations = parse_recording_relations([
            "spotify:track:track:with:colon=12345678-1234-1234-1234-123456789abc",
        ])
        .unwrap();
        assert_eq!(relations.len(), 1);
        assert_eq!(relations[0].media_id.raw_id, "track:with:colon");
        assert!(parse_recording_relations(["spotify:track:=mbid"]).is_err());
        assert!(parse_recording_relations(["unknown:track:id=mbid"]).is_err());
        assert!(parse_recording_relations(["spotify:unknown:id=mbid"]).is_err());
        assert!(parse_recording_relations(["spotify:track:id"]).is_err());
    }

    #[test]
    fn unified_remove_selects_one_occurrence_or_explicitly_all_duplicates() {
        let playlist = crate::state::UnifiedPlaylist {
            items: vec![
                projection_item(crate::state::Provider::Spotify, "same", "One", "Artist"),
                projection_item(crate::state::Provider::Spotify, "same", "Two", "Artist"),
            ],
            ..crate::state::UnifiedPlaylist::default()
        };
        let mut playlist = playlist;
        playlist.items[0].entry_id = crate::state::PlaylistEntryId(10);
        playlist.items[1].entry_id = crate::state::PlaylistEntryId(20);
        assert_eq!(
            unified_remove_entry_ids(&playlist, Some(20), None, false).unwrap(),
            vec![crate::state::PlaylistEntryId(20)]
        );
        assert_eq!(
            unified_remove_entry_ids(&playlist, None, Some("same"), true).unwrap(),
            vec![
                crate::state::PlaylistEntryId(10),
                crate::state::PlaylistEntryId(20)
            ]
        );
        assert!(unified_remove_entry_ids(&playlist, None, Some("same"), false).is_err());
    }

    fn youtube_candidate(id: &str, title: &str, artists: &str) -> crate::state::YouTubeTrack {
        crate::state::YouTubeTrack {
            id: id.to_owned(),
            name: title.to_owned(),
            artists: artists.to_owned(),
            album: None,
            duration: "3:49".to_owned(),
            explicit: false,
            thumbnail_url: None,
            is_video: false,
        }
    }

    #[test]
    fn diagnostic_failure_does_not_echo_server_payload() {
        let private_payload = "provider response with https://secret.example/token";
        let error = diagnostic_response_text(Response::Err(private_payload.as_bytes().to_vec()))
            .unwrap_err()
            .to_string();

        assert_eq!(
            error,
            "live diagnostic request failed; inspect local application logs"
        );
        assert!(!error.contains(private_payload));
    }

    #[test]
    fn listenbrainz_description_restore_preserves_provider_occurrences() {
        let playlist = crate::state::UnifiedPlaylist {
            id: "portable".to_owned(),
            name: "Portable".to_owned(),
            items: vec![
                crate::state::UnifiedPlaylistItem {
                    entry_id: crate::state::PlaylistEntryId(3),
                    media_id: crate::state::MediaId {
                        provider: crate::state::Provider::YouTubeMusic,
                        kind: crate::state::MediaKind::Video,
                        raw_id: "video".to_owned(),
                    },
                    title: "Video".to_owned(),
                    artists: "Artist".to_owned(),
                    ..crate::state::UnifiedPlaylistItem::default()
                },
                crate::state::UnifiedPlaylistItem {
                    entry_id: crate::state::PlaylistEntryId(8),
                    media_id: crate::state::MediaId {
                        provider: crate::state::Provider::Spotify,
                        kind: crate::state::MediaKind::Episode,
                        raw_id: "episode".to_owned(),
                    },
                    title: "Episode".to_owned(),
                    artists: "Publisher".to_owned(),
                    ..crate::state::UnifiedPlaylistItem::default()
                },
            ],
            next_entry_id: 9,
            ..crate::state::UnifiedPlaylist::default()
        };
        let value = serde_json::json!({
            "playlist": {
                "title": playlist.name,
                "annotation": description_envelope(&playlist).unwrap()
            }
        });

        let (name, restored, unresolved) = parse_listenbrainz_playlist(&value).unwrap();

        assert_eq!(name, "Portable");
        assert_eq!(unresolved, 2);
        assert_eq!(restored.len(), 2);
        assert_eq!(restored[0].entry_id, crate::state::PlaylistEntryId(3));
        assert_eq!(
            restored[0].media_id.provider,
            crate::state::Provider::YouTubeMusic
        );
        assert_eq!(restored[0].media_id.kind, crate::state::MediaKind::Video);
        assert_eq!(restored[1].entry_id, crate::state::PlaylistEntryId(8));
        assert_eq!(restored[1].media_id.kind, crate::state::MediaKind::Episode);
        assert!(restored.iter().all(|item| item.playable_media().is_some()));
    }

    #[test]
    fn parses_provider_extensions_and_fallback_identifiers() {
        let value = serde_json::json!({
            "playlist": {
                "title": "Mixed",
                "track": [
                    {
                        "title": "Spotify song",
                        "creator": "Artist",
                        "identifier": ["spotify:track:abc"],
                        "extension": {
                            "unified-player:provider": "Spotify",
                            "unified-player:raw_id": "abc",
                            "unified-player:entry_id": 7,
                            "unified-player:metadata_provenance": "provider",
                            "unified-player:metadata_observed_at": 42
                        }
                    },
                    {
                        "title": "YouTube song",
                        "identifier": ["https://music.youtube.com/watch?v=xyz"]
                    },
                    { "title": "Unknown", "identifier": ["https://example.test/track"] },
                    { "title": "No identifier", "creator": "Unknown" }
                ]
            }
        });
        let (name, items, unresolved) = parse_listenbrainz_playlist(&value).unwrap();
        assert_eq!(name, "Mixed");
        assert_eq!(items.len(), 4);
        assert_eq!(items[0].media_id.raw_id, "abc");
        assert_eq!(items[0].entry_id, crate::state::PlaylistEntryId(7));
        assert_eq!(items[0].metadata.source_entry_id, Some(7));
        assert_eq!(items[0].metadata.provenance.as_deref(), Some("provider"));
        assert_eq!(items[0].metadata.observed_at, Some(42));
        assert_eq!(items[1].media_id.raw_id, "xyz");
        assert_eq!(
            items[2].metadata.source_identifier.as_deref(),
            Some("https://example.test/track")
        );
        assert_eq!(
            items[2].metadata.provenance.as_deref(),
            Some("unresolved-listenbrainz")
        );
        assert!(items[2].playable_media().is_none());
        assert_eq!(items[3].media_id.raw_id, "unresolved:3");
        assert_eq!(items[3].metadata.source_identifier, None);
        assert_eq!(unresolved, 4);
    }

    #[test]
    fn parses_explicit_duration_units_and_media_kind_extensions() {
        let value = serde_json::json!({
            "playlist": {
                "title": "Units",
                "track": [{
                    "title": "Video",
                    "creator": "Artist",
                    "duration": 62,
                    "identifier": ["https://music.youtube.com/watch?v=xyz"],
                    "extension": {
                        "unified-player:duration_unit": "seconds",
                        "unified-player:media_kind": "Video"
                    }
                }]
            }
        });
        let (_, items, unresolved) = parse_listenbrainz_playlist(&value).unwrap();
        assert_eq!(unresolved, 0);
        assert_eq!(items[0].duration_ms, Some(62_000));
        assert_eq!(
            items[0].duration_unit,
            crate::state::DurationUnit::Milliseconds
        );
        assert_eq!(items[0].media_id.kind, crate::state::MediaKind::Video);
    }

    #[test]
    fn jspf_export_and_listenbrainz_parser_preserve_portable_identity() {
        let playlist = crate::state::UnifiedPlaylist {
            id: "portable".to_owned(),
            name: "Portable".to_owned(),
            items: vec![
                crate::state::UnifiedPlaylistItem {
                    entry_id: crate::state::PlaylistEntryId(11),
                    media_id: crate::state::MediaId {
                        provider: crate::state::Provider::YouTubeMusic,
                        kind: crate::state::MediaKind::Video,
                        raw_id: "video".to_owned(),
                    },
                    title: "Video".to_owned(),
                    artists: "Artist".to_owned(),
                    duration_ms: Some(62_000),
                    provider_url: Some("https://music.youtube.com/watch?v=video".to_owned()),
                    ..crate::state::UnifiedPlaylistItem::default()
                },
                crate::state::UnifiedPlaylistItem {
                    entry_id: crate::state::PlaylistEntryId(12),
                    media_id: crate::state::MediaId {
                        provider: crate::state::Provider::Spotify,
                        kind: crate::state::MediaKind::Track,
                        raw_id: "track".to_owned(),
                    },
                    title: "Track".to_owned(),
                    artists: "Artist".to_owned(),
                    duration_ms: Some(180_000),
                    provider_url: Some("spotify:track:track".to_owned()),
                    ..crate::state::UnifiedPlaylistItem::default()
                },
            ],
            updated_at: 0,
            next_entry_id: 13,
        };
        let (_, imported, unresolved) =
            parse_listenbrainz_playlist(&playlist.to_jspf_value()).unwrap();
        assert_eq!(unresolved, 0);
        assert_eq!(imported.len(), 2);
        assert_eq!(imported[0].entry_id, crate::state::PlaylistEntryId(11));
        assert_eq!(imported[0].media_id.kind, crate::state::MediaKind::Video);
        assert_eq!(imported[0].duration_ms, Some(62_000));
        assert_eq!(imported[1].entry_id, crate::state::PlaylistEntryId(12));
        assert_eq!(
            imported[1].media_id.provider,
            crate::state::Provider::Spotify
        );
        assert_eq!(
            imported[1].duration_unit,
            crate::state::DurationUnit::Milliseconds
        );
    }

    #[test]
    fn projection_only_appends_missing_duplicate_occurrences() {
        let desired = vec!["a".to_owned(), "a".to_owned(), "b".to_owned()];
        let existing = vec!["a".to_owned()];
        assert_eq!(
            missing_youtube_ids(&desired, &existing),
            vec!["a".to_owned(), "b".to_owned()]
        );
    }

    #[test]
    fn normal_and_merge_resolution_never_search_or_rewrite_spotify_items() {
        let playlist = crate::state::UnifiedPlaylist {
            id: "local".to_owned(),
            name: "Mixed".to_owned(),
            items: vec![
                projection_item(
                    crate::state::Provider::YouTubeMusic,
                    "yt-1",
                    "Native",
                    "Artist",
                ),
                projection_item(crate::state::Provider::Spotify, "sp-1", "Missing", "Artist"),
            ],
            updated_at: 0,
            next_entry_id: 1,
        };
        for resolution in [None, Some("local"), Some("merge")] {
            let result = resolve_projection_items(&playlist, resolution, |_| {
                panic!("normal projection unexpectedly searched YouTube")
            })
            .unwrap();
            assert_eq!(
                result,
                ProjectionResolution {
                    desired_youtube_ids: vec!["yt-1".to_owned()],
                    unresolved: vec!["Spotify:sp-1".to_owned()],
                    matched: Vec::new(),
                    accepted_mappings: Vec::new(),
                }
            );
        }
    }

    #[test]
    fn match_resolution_returns_safe_ids_for_dry_run_and_apply() {
        let playlist = crate::state::UnifiedPlaylist {
            id: "local".to_owned(),
            name: "Mixed".to_owned(),
            items: vec![projection_item(
                crate::state::Provider::Spotify,
                "sp-1",
                "Want Some More",
                "Nicki Minaj",
            )],
            updated_at: 0,
            next_entry_id: 1,
        };
        let result = resolve_projection_items(&playlist, Some("match"), |_| {
            Ok(vec![youtube_candidate(
                "yt-1",
                "Want Some More",
                "Nicki Minaj",
            )])
        })
        .unwrap();
        assert_eq!(result.desired_youtube_ids, vec!["yt-1"]);
        assert!(result.unresolved.is_empty());
        assert_eq!(result.matched.len(), 1);
        assert_eq!(result.accepted_mappings.len(), 1);
        assert_eq!(
            result.accepted_mappings[0].local_entry_id,
            playlist.items[0].entry_id
        );
        assert_eq!(result.accepted_mappings[0].remote_media_id.raw_id, "yt-1");
    }

    #[test]
    fn match_resolution_fails_closed_on_ambiguity_and_no_match() {
        let playlist = crate::state::UnifiedPlaylist {
            id: "local".to_owned(),
            name: "Mixed".to_owned(),
            items: vec![
                projection_item(
                    crate::state::Provider::Spotify,
                    "sp-ambiguous",
                    "Want Some More",
                    "Nicki Minaj",
                ),
                projection_item(
                    crate::state::Provider::Spotify,
                    "sp-missing",
                    "Not There",
                    "Unknown",
                ),
            ],
            updated_at: 0,
            next_entry_id: 1,
        };
        let result = resolve_projection_items(&playlist, Some("match"), |item| {
            if item.media_id.raw_id == "sp-ambiguous" {
                Ok(vec![
                    youtube_candidate("yt-a", "Want Some More (Live)", "Nicki Minaj"),
                    youtube_candidate("yt-b", "Want Some More (Remix)", "Nicki Minaj"),
                ])
            } else {
                Ok(Vec::new())
            }
        })
        .unwrap();
        assert!(result.desired_youtube_ids.is_empty());
        assert_eq!(result.unresolved.len(), 2);
        assert!(result.matched.is_empty());
    }

    #[test]
    fn unresolved_match_is_allowed_in_dry_run_but_blocks_apply() {
        let unresolved = vec!["Spotify:sp-1 (Missing)".to_owned()];
        assert!(ensure_projection_resolution_complete(Some("match"), false, &unresolved).is_ok());
        let error = ensure_projection_resolution_complete(Some("match"), true, &unresolved)
            .unwrap_err()
            .to_string();
        assert!(error.contains("no changes were written"));
        assert!(ensure_projection_resolution_complete(Some("local"), true, &unresolved).is_ok());
    }
}

/// Tries to connect to a running client, if exists, by sending a connection request
/// to the client via a UDP socket.
/// If no running client found, create a new client running in a separate thread to
/// handle the socket request.
fn try_connect_to_client(socket: &UdpSocket, configs: &config::Configs) -> Result<()> {
    let port = configs.app_config.client_port;
    socket.connect(("127.0.0.1", port))?;

    // send an empty buffer as a connection request to the client
    socket.send(&[])?;
    if let Err(err) = socket.recv(&mut [0; 1]) {
        if let std::io::ErrorKind::ConnectionRefused = err.kind() {
            // no running `unified-player` instance found,
            // initialize a new client to handle the current CLI command

            let rt = tokio::runtime::Runtime::new()?;

            // create a Spotify API client
            let client = rt
                .block_on(client::AppClient::new())
                .context("construct app client")?;
            rt.block_on(client.new_session(None, false))
                .context("new session")?;

            // create a client socket for handling CLI commands
            // NOTE: the socket must be bound *before* spawning the thread to avoid a
            // race condition where the caller sends a request before the socket is ready.
            let client_socket = rt.block_on(tokio::net::UdpSocket::bind(("127.0.0.1", port)))?;

            // spawn a thread to handle the CLI request
            std::thread::spawn(move || {
                rt.block_on(start_socket(&client, None, Some(client_socket)));
            });
        } else {
            return Err(err.into());
        }
    }

    Ok(())
}

pub fn handle_cli_subcommand(cmd: &str, args: &ArgMatches) -> Result<()> {
    let configs = config::get_config();

    // handle commands that don't require a client separately
    match cmd {
        "demo" => return handle_demo_subcommand(args, configs),
        "youtube" => return handle_youtube_subcommand(args, configs),
        "unified" => return handle_unified_subcommand(args, configs),
        "listenbrainz" => return handle_listenbrainz_subcommand(args, configs),
        "authenticate" => {
            // Force re-authentication of both credentials the application relies on:
            // the Web API token and the librespot session credentials.
            // Each runs its own interactive OAuth flow under a different client ID.
            let mut api_client = client::new_api_client()?;
            let prompt = crate::cli::TerminalAuthPrompt;
            let rt = tokio::runtime::Runtime::new()?;
            rt.block_on(crate::auth::prompt_for_user_token_with_interaction(
                &mut api_client,
                true,
                crate::auth::AuthInteraction::Prompt(&prompt),
            ))
            .context("authenticate Spotify Web API client")?;

            let auth_config = AuthConfig::new(configs)?;
            crate::auth::get_creds_with_interaction(
                &auth_config,
                true,
                false,
                crate::auth::AuthInteraction::Prompt(&prompt),
            )?;
            std::process::exit(0);
        }
        "generate" => {
            let gen = *args
                .get_one::<Shell>("shell")
                .expect("shell argument is required");
            let mut cmd = init_cli()?;
            let name = cmd.get_name().to_string();
            generate(gen, &mut cmd, name, &mut std::io::stdout());
            std::process::exit(0);
        }
        "features" => {
            print_features();
            std::process::exit(0);
        }
        "diagnostics" if !args.get_flag("live") => {
            if let Some(output) = args.get_one::<std::path::PathBuf>("bundle") {
                let source = configs
                    .app_config
                    .log_folder
                    .as_deref()
                    .context("diagnostic log folder is unavailable")?;
                let review = crate::observability::create_support_bundle(source, output)?;
                print_bundle_review(&review, true);
                return Ok(());
            }
            if let Some(bundle) = args.get_one::<std::path::PathBuf>("review_bundle") {
                let review = crate::observability::review_support_bundle(bundle)?;
                print_bundle_review(&review, false);
                return Ok(());
            }
            if args.get_flag("trend") {
                let source = configs
                    .app_config
                    .log_folder
                    .as_deref()
                    .context("diagnostic log folder is unavailable")?;
                print!("{}", crate::observability::render_local_trend(source)?);
                return Ok(());
            }
            print!("{}", super::diagnostics::render(configs, None));
            return Ok(());
        }
        "diagnostics" => {
            return print_live_diagnostics(configs, args.get_one::<u16>("verbose_seconds").copied())
        }
        _ => {}
    }

    let socket = UdpSocket::bind("127.0.0.1:0")?;
    try_connect_to_client(&socket, configs).context("try to connect to a client")?;

    // construct a socket request based on the CLI command and its arguments
    let request = match cmd {
        "get" => handle_get_subcommand(args),
        "playback" => handle_playback_subcommand(args)?,
        "playlist" => handle_playlist_subcommand(args)?,
        "connect" => Request::Connect(get_id_or_name(args)),
        "like" => Request::Like {
            unlike: args.get_flag("unlike"),
        },
        "search" => Request::Search {
            query: args
                .get_one::<String>("query")
                .expect("query is required")
                .to_owned(),
        },
        "lyrics" => Request::Lyrics {
            id_or_name: try_get_id_or_name(args),
        },
        _ => unreachable!(),
    };

    // send the request to the client's socket
    let request_buf = serde_json::to_vec(&request)?;
    assert!(request_buf.len() <= MAX_REQUEST_SIZE);
    socket.send(&request_buf)?;

    // receive and handle a response from the client's socket
    match receive_response(&socket)? {
        Response::Err(err) => {
            eprintln!("{}", String::from_utf8_lossy(&err));
            std::process::exit(1);
        }
        Response::Ok(data) => {
            println!("{}", String::from_utf8_lossy(&data).replace("\\n", "\n"));
            std::process::exit(0);
        }
    }
}

fn print_bundle_review(review: &crate::observability::BundleReview, created: bool) {
    println!("bundle.created={created}");
    println!("bundle.manifest_version=1");
    println!("bundle.files={}", review.files.join(","));
    println!("bundle.events={}", review.event_count);
    println!("bundle.checksums=verified");
    println!("bundle.forbidden_findings={}", review.forbidden_findings);
    println!("bundle.backtraces=excluded");
    println!("bundle.remote_export=disabled");
    println!("bundle.review=complete");
}

fn print_live_diagnostics(configs: &config::Configs, verbose_seconds: Option<u16>) -> Result<()> {
    let socket = UdpSocket::bind("127.0.0.1:0")?;
    socket.connect(("127.0.0.1", configs.app_config.client_port))?;
    socket.set_read_timeout(Some(std::time::Duration::from_millis(500)))?;
    socket.send(&[])?;
    socket
        .recv(&mut [0; 1])
        .context("no running unified-player application answered the diagnostic probe")?;
    let request = verbose_seconds.map_or(Request::Diagnostics, |seconds| {
        Request::DiagnosticsFilter { seconds }
    });
    socket.send(&serde_json::to_vec(&request)?)?;
    print!("{}", diagnostic_response_text(receive_response(&socket)?)?);
    Ok(())
}

fn diagnostic_response_text(response: Response) -> Result<String> {
    match response {
        Response::Ok(data) => {
            String::from_utf8(data).context("live diagnostic response was not valid UTF-8")
        }
        Response::Err(_) => {
            anyhow::bail!("live diagnostic request failed; inspect local application logs")
        }
    }
}

fn handle_playlist_subcommand(args: &ArgMatches) -> Result<Request> {
    let (cmd, args) = args.subcommand().expect("playlist subcommand is required");
    let command = match cmd {
        "new" => {
            let name = args
                .get_one::<String>("name")
                .expect("name arg is required")
                .to_owned();

            let description = args
                .get_one::<String>("description")
                .map(std::borrow::ToOwned::to_owned)
                .unwrap_or_default();

            let public = args.get_flag("public");
            let collab = args.get_flag("collab");

            PlaylistCommand::New {
                name,
                public,
                collab,
                description,
            }
        }
        "delete" => {
            let id = args
                .get_one::<String>("id")
                .expect("id arg is required")
                .to_owned();

            let pid = PlaylistId::from_id(id)?;

            PlaylistCommand::Delete { id: pid }
        }
        "list" => PlaylistCommand::List,
        "import" => {
            let from_s = args
                .get_one::<String>("from")
                .expect("'from' PlaylistID is required.")
                .to_owned();

            let to_s = args
                .get_one::<String>("to")
                .expect("'to' PlaylistID is required.")
                .to_owned();

            let delete = args.get_flag("delete");

            let from = PlaylistId::from_id(from_s.clone())?;
            let to = PlaylistId::from_id(to_s.clone())?;

            println!("Importing '{from_s}' into '{to_s}'...\n");
            PlaylistCommand::Import { from, to, delete }
        }
        "fork" => {
            let id_s = args
                .get_one::<String>("id")
                .expect("Playlist id is required.")
                .to_owned();

            let id = PlaylistId::from_id(id_s.clone())?;

            println!("Forking '{id_s}'...\n");
            PlaylistCommand::Fork { id }
        }
        "sync" => {
            let id_s = args.get_one::<String>("id");
            let delete = args.get_flag("delete");

            let pid = if let Some(id_s) = id_s {
                println!("Syncing imports for playlist '{id_s}'...\n");
                Some(PlaylistId::from_id(id_s.to_owned())?)
            } else {
                println!("Syncing imports for all playlists...\n");
                None
            };

            PlaylistCommand::Sync { id: pid, delete }
        }
        "edit" => {
            let playlist_id = PlaylistId::from_id(
                args.get_one::<String>("playlist_id")
                    .expect("playlist_id arg is required")
                    .to_owned(),
            )?;

            let action = *args
                .get_one::<EditAction>("action")
                .expect("action arg is required");

            let track_id = args
                .get_one::<String>("track_id")
                .map(|s| TrackId::from_id(s.to_owned()))
                .transpose()?;

            let album_id = args
                .get_one::<String>("album_id")
                .map(|s| AlbumId::from_id(s.to_owned()))
                .transpose()?;

            PlaylistCommand::Edit {
                playlist_id,
                action,
                track_id,
                album_id,
            }
        }
        _ => unreachable!(),
    };

    Ok(Request::Playlist(command))
}

macro_rules! print_feature {
    ($feature:literal) => {
        #[cfg(feature = $feature)]
        println!("  ✓ {}", $feature);
        #[cfg(not(feature = $feature))]
        println!("  ✗ {}", $feature);
    };
}

fn print_features() {
    println!("Compile-time features:");

    print_feature!("daemon");
    print_feature!("streaming");
    print_feature!("media-control");
    print_feature!("image");
    print_feature!("ratatui-image");
    print_feature!("sixel");
    print_feature!("pixelate");
    print_feature!("notify");
    print_feature!("fzf");

    // Audio backends
    print_feature!("pulseaudio-backend");
    print_feature!("alsa-backend");
    print_feature!("rodio-backend");
    print_feature!("jackaudio-backend");
    print_feature!("sdl-backend");
    print_feature!("gstreamer-backend");
}
