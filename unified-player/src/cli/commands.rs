use clap::{builder::EnumValueParser, value_parser, Arg, ArgAction, ArgGroup, Command};
use clap_complete::Shell;

use crate::cli::EditAction;

use super::{ContextType, ItemType, Key};

pub fn init_connect_subcommand() -> Command {
    add_id_or_name_group(Command::new("connect").about("Connect to a Spotify device"))
}

pub fn init_get_subcommand() -> Command {
    Command::new("get")
        .about("Get Spotify data")
        .subcommand_required(true)
        .subcommand(
            Command::new("key").about("Get data by key").arg(
                Arg::new("key")
                    .value_parser(EnumValueParser::<Key>::new())
                    .required(true),
            ),
        )
        .subcommand(add_id_or_name_group(
            Command::new("item").about("Get a Spotify item's data").arg(
                Arg::new("item_type")
                    .value_parser(EnumValueParser::<ItemType>::new())
                    .required(true),
            ),
        ))
}

fn init_playback_start_subcommand() -> Command {
    Command::new("start")
        .about("Start a new playback")
        .subcommand_required(true)
        .subcommand(add_id_or_name_group(
            Command::new("context")
                .about("Start a context playback")
                .arg(
                    Arg::new("context_type")
                        .value_parser(EnumValueParser::<ContextType>::new())
                        .required(true),
                )
                .arg(
                    Arg::new("shuffle")
                        .short('s')
                        .long("shuffle")
                        .action(ArgAction::SetTrue)
                        .help("Shuffle tracks within the launched playback"),
                ),
        ))
        .subcommand(add_id_or_name_group(
            Command::new("track").about("Start playback for a track"),
        ))
        .subcommand(
            Command::new("liked")
                .about("Start a liked tracks playback")
                .arg(
                    Arg::new("limit")
                        .short('l')
                        .long("limit")
                        .default_value("200")
                        .value_parser(value_parser!(usize))
                        .help("The limit for number of tracks to play"),
                )
                .arg(
                    Arg::new("random")
                        .short('r')
                        .long("random")
                        .action(ArgAction::SetTrue)
                        .help(
                            "Randomly pick the tracks instead of picking tracks from the beginning",
                        ),
                ),
        )
        .subcommand(add_id_or_name_group(
            Command::new("radio")
                .about("Start a radio playback")
                .arg(Arg::new("item_type").value_parser(EnumValueParser::<ItemType>::new())),
        ))
}

fn add_id_or_name_group(cmd: Command) -> Command {
    add_id_or_name_group_optional(cmd, true)
}

fn add_id_or_name_group_optional(cmd: Command, required: bool) -> Command {
    cmd.arg(Arg::new("id").long("id").short('i'))
        .arg(Arg::new("name").long("name").short('n'))
        .group(
            ArgGroup::new("id_or_name")
                .args(["id", "name"])
                .required(required),
        )
}

pub fn init_playback_subcommand() -> Command {
    Command::new("playback")
        .about("Interact with the playback")
        .subcommand_required(true)
        .subcommand(init_playback_start_subcommand())
        .subcommand(Command::new("play-pause").about("Toggle between play and pause"))
        .subcommand(Command::new("play").about("Resume the current playback if stopped"))
        .subcommand(Command::new("pause").about("Pause the current playback if playing"))
        .subcommand(Command::new("next").about("Skip to the next track"))
        .subcommand(Command::new("previous").about("Skip to the previous track"))
        .subcommand(Command::new("shuffle").about("Toggle the shuffle mode"))
        .subcommand(Command::new("repeat").about("Cycle the repeat mode"))
        .subcommand(
            Command::new("volume")
                .about("Set the volume percentage")
                .arg(
                    Arg::new("percent")
                        .value_parser(value_parser!(i8).range(-100..=100))
                        .required(true),
                )
                .arg(
                    Arg::new("offset")
                        .long("offset")
                        .action(clap::ArgAction::SetTrue)
                        .help("Increase the volume percent by an offset"),
                ),
        )
        .subcommand(
            Command::new("seek")
                .about("Seek by an offset milliseconds")
                .arg(
                    Arg::new("position_offset_ms")
                        .value_parser(value_parser!(i64))
                        .required(true),
                ),
        )
}

pub fn init_search_command() -> Command {
    Command::new("search")
        .about("Search spotify")
        .arg(Arg::new("query").help("Search query").required(true))
}

pub fn init_like_command() -> Command {
    Command::new("like")
        .about("Like currently playing track")
        .arg(
            Arg::new("unlike")
                .long("unlike")
                .short('u')
                .action(ArgAction::SetTrue)
                .help("Unlike the currently playing track"),
        )
}

pub fn init_authenticate_command() -> Command {
    Command::new("authenticate").about("Authenticate the application")
}

pub fn init_demo_command() -> Command {
    Command::new("demo")
        .about("Render offline UI demos without starting providers or playback")
        .subcommand_required(true)
        .subcommand(
            Command::new("screen")
                .about("Render a workspace screen from synthetic data at one or more sizes")
                .arg(
                    Arg::new("screen")
                        .value_name("SCREEN")
                        .required(true)
                        .value_parser(crate::ui::PreviewScreen::NAMES),
                )
                .arg(
                    Arg::new("scenario")
                        .long("scenario")
                        .value_name("SCENARIO")
                        .default_value("ready")
                        .value_parser(crate::ui::PreviewScenario::NAMES),
                )
                .arg(
                    Arg::new("size")
                        .long("size")
                        .value_name("WIDTHxHEIGHT")
                        .action(ArgAction::Append)
                        .help("Repeatable; defaults to 80x24, 120x35 and 180x49"),
                )
                .arg(
                    Arg::new("interactive")
                        .long("interactive")
                        .action(ArgAction::SetTrue)
                        .conflicts_with("size")
                        .help("Open the screen in this terminal with the real key/mouse handlers"),
                )
                .arg(
                    Arg::new("color")
                        .long("color")
                        .value_name("WHEN")
                        .default_value("auto")
                        .value_parser(["auto", "always", "never"])
                        .help("ANSI colors; `auto` colors only when stdout is a terminal"),
                ),
        )
        .subcommand(
            Command::new("welcome")
                .about("Render the real first-use Welcome page with synthetic state")
                .arg(
                    Arg::new("interactive")
                        .long("interactive")
                        .action(ArgAction::SetTrue)
                        .help("Open a safe keyboard/mouse demo in the current terminal"),
                )
                .arg(
                    Arg::new("layout")
                        .long("layout")
                        .default_value("sidebar")
                        .value_parser(["classic", "centered", "sidebar"])
                        .help("Compare Welcome layouts; compact terminals use the shared list"),
                )
                .arg(
                    Arg::new("scenario")
                        .long("scenario")
                        .value_name("SCENARIO")
                        .default_value("fresh")
                        .value_parser([
                            "fresh",
                            "spotify-ready",
                            "youtube-browser-ready",
                            "youtube-oauth-ready",
                            "auth-failed",
                            "spotify-checking",
                            "youtube-checking",
                            "spotify-restart-required",
                            "spotify-cached",
                            "youtube-waiting",
                            "spotify-rate-limited",
                        ]),
                )
                .arg(
                    Arg::new("step")
                        .long("step")
                        .value_name("STEP")
                        .default_value("preferences")
                        .value_parser([
                            "preferences",
                            "spotify",
                            "youtube",
                            "listenbrainz",
                            "review",
                        ]),
                )
                .arg(
                    Arg::new("width")
                        .long("width")
                        .value_name("COLUMNS")
                        .default_value("100")
                        .value_parser(value_parser!(u16).range(40..=240)),
                )
                .arg(
                    Arg::new("height")
                        .long("height")
                        .value_name("ROWS")
                        .default_value("26")
                        .value_parser(value_parser!(u16).range(16..=80)),
                ),
        )
}

pub fn init_youtube_subcommand() -> Command {
    let command = Command::new("youtube")
        .about("YouTube Music setup and diagnostics")
        .subcommand_required(true)
        .subcommand(
            Command::new("auth")
                .about("Show guided YouTube Music authentication setup")
                .arg(
                    Arg::new("auth_type")
                        .long("type")
                        .value_name("TYPE")
                        .value_parser(["browser", "oauth"])
                        .help("Show setup for browser-cookie or OAuth authentication"),
                ),
        )
        .subcommand(
            Command::new("login")
                .about("Sign in to YouTube Music once with Google's device flow")
                .arg(
                    Arg::new("client_id")
                        .long("client-id")
                        .value_name("CLIENT_ID")
                        .help("Google TVs and Limited Input devices OAuth client ID"),
                )
                .arg(
                    Arg::new("client_secret")
                        .long("client-secret")
                        .value_name("CLIENT_SECRET")
                        .help("Google OAuth client secret; prefer the environment variable"),
                )
                .arg(
                    Arg::new("no_open")
                        .long("no-open")
                        .action(ArgAction::SetTrue)
                        .help("Print the verification URL without opening a browser"),
                ),
        )
        .subcommand(
            Command::new("browser-login")
                .about("Sign in once with a dedicated browser profile for playback")
                .arg(
                    Arg::new("browser")
                        .long("browser")
                        .value_name("PATH")
                        .value_parser(value_parser!(std::path::PathBuf))
                        .help("Chrome, Chromium, or Edge executable path"),
                )
                .arg(
                    Arg::new("keep_open")
                        .long("keep-open")
                        .action(ArgAction::SetTrue)
                        .help("Leave the dedicated browser open after importing its session"),
                ),
        )
        .subcommand(
            Command::new("status")
                .about("Show YouTube Music credential status")
                .arg(
                    Arg::new("check")
                        .long("check")
                        .action(ArgAction::SetTrue)
                        .help("Make authenticated library requests to verify the credential"),
                )
                .arg(
                    Arg::new("video_id")
                        .long("video-id")
                        .value_name("VIDEO_ID")
                        .requires("check")
                        .help("Resolve this video/song instead of the public playback probe"),
                )
                .arg(
                    Arg::new("audio_output")
                        .long("audio-output")
                        .action(ArgAction::SetTrue)
                        .requires("check")
                        .help("Silently verify the default audio device and Rodio output sink"),
                )
                .arg(
                    Arg::new("transport_diagnostic")
                        .long("transport-diagnostic")
                        .action(ArgAction::SetTrue)
                        .requires("check")
                        .requires("video_id")
                        .conflicts_with("audio_output")
                        .help(
                            "Compare browser capture and native media replay without printing credentials",
                        ),
                ),
        )
        .subcommand(
            Command::new("probe")
                .about("Resolve and media-probe one YouTube video without starting playback")
                .arg(
                    Arg::new("video_id")
                        .long("video-id")
                        .value_name("VIDEO_ID")
                        .required(true),
                )
                .arg(
                    Arg::new("json")
                        .long("json")
                        .action(ArgAction::SetTrue)
                        .help("Print the redacted probe report as JSON"),
                )
                .arg(
                    Arg::new("client")
                        .long("client")
                        .value_name("CLIENT")
                        .default_value("auto")
                        .value_parser([
                            "auto",
                            "tv",
                            "web",
                            "web-remix",
                            "android-vr",
                            "visionos",
                        ])
                        .help("Probe one native player client instead of the automatic route"),
                )
                .arg(
                    Arg::new("decoder_chunk_size")
                        .long("decoder-chunk-size")
                        .value_name("SIZE")
                        .default_value("1m")
                        .value_parser(["1m", "10m"])
                        .help("Use a bounded decoder range size for this probe only"),
                )
                .arg(
                    Arg::new("allow_browser_fallback")
                        .long("allow-browser-fallback")
                        .action(ArgAction::SetTrue)
                        .help("Allow the dedicated Chrome/Chromium playback fallback"),
                ),
        )
        .subcommand(
            Command::new("like")
                .about("Like or unlike a YouTube Music video/song")
                .arg(Arg::new("video_id").long("video-id").required(true))
                .arg(Arg::new("unlike").long("unlike").action(ArgAction::SetTrue)),
        )
        .subcommand(
            Command::new("playlist")
                .about("Mutate an owned YouTube Music playlist")
                .subcommand_required(true)
                .subcommand(
                    Command::new("create")
                        .arg(Arg::new("name").required(true))
                        .arg(Arg::new("public").long("public").action(ArgAction::SetTrue)),
                )
                .subcommand(
                    Command::new("add")
                        .arg(Arg::new("playlist_id").long("playlist-id").required(true))
                        .arg(Arg::new("video_id").long("video-id").required(true)),
                )
                .subcommand(
                    Command::new("remove")
                        .arg(Arg::new("playlist_id").long("playlist-id").required(true))
                        .arg(Arg::new("set_video_id").long("set-video-id").required(true)),
                )
                .subcommand(
                    Command::new("delete")
                        .arg(Arg::new("playlist_id").long("playlist-id").required(true)),
                ),
        );

    #[cfg(feature = "private-capture")]
    let command = command.subcommand(init_youtube_debug_command());

    #[cfg(feature = "private-capture")]
    let command = command.subcommand(init_private_capture_command());

    command
}

#[cfg(feature = "private-capture")]
fn init_youtube_debug_command() -> Command {
    Command::new("debug")
        .about("Inspect YouTube provider responses in a private terminal")
        .long_about(
            "Inspect one YouTube account without starting the TUI. Output is private developer \
             evidence: it contains provider response facts, but masks credentials and signed URL \
             values and cannot be redirected.",
        )
        .subcommand_required(true)
        .subcommand(
            Command::new("inspect")
                .about("Inspect one account against one video")
                .arg(
                    Arg::new("video_id")
                        .long("video-id")
                        .value_name("VIDEO_ID")
                        .required(true),
                )
                .arg(
                    Arg::new("transport")
                        .long("transport")
                        .action(ArgAction::SetTrue)
                        .help("Also exercise the dedicated-browser media transport diagnostic"),
                )
                .arg(
                    Arg::new("json")
                        .long("json")
                        .action(ArgAction::SetTrue)
                        .help("Render the private normalized report as JSON"),
                )
                .arg(
                    Arg::new("acknowledge_sensitive")
                        .long("acknowledge-sensitive")
                        .action(ArgAction::SetTrue)
                        .required(true)
                        .help("Acknowledge that this report is private provider evidence"),
                ),
        )
        .subcommand(
            Command::new("compare")
                .about("Compare two isolated account config folders")
                .arg(
                    Arg::new("config_a")
                        .long("config-a")
                        .value_name("FOLDER")
                        .required(true)
                        .value_parser(value_parser!(std::path::PathBuf)),
                )
                .arg(
                    Arg::new("cache_a")
                        .long("cache-a")
                        .value_name("FOLDER")
                        .required(true)
                        .value_parser(value_parser!(std::path::PathBuf)),
                )
                .arg(
                    Arg::new("config_b")
                        .long("config-b")
                        .value_name("FOLDER")
                        .required(true)
                        .value_parser(value_parser!(std::path::PathBuf)),
                )
                .arg(
                    Arg::new("cache_b")
                        .long("cache-b")
                        .value_name("FOLDER")
                        .required(true)
                        .value_parser(value_parser!(std::path::PathBuf)),
                )
                .arg(
                    Arg::new("video_id")
                        .long("video-id")
                        .value_name("VIDEO_ID")
                        .required(true),
                )
                .arg(
                    Arg::new("transport")
                        .long("transport")
                        .action(ArgAction::SetTrue)
                        .help("Also exercise browser media transport for both accounts"),
                )
                .arg(
                    Arg::new("json")
                        .long("json")
                        .action(ArgAction::SetTrue)
                        .help("Render the private comparison as JSON"),
                )
                .arg(
                    Arg::new("acknowledge_sensitive")
                        .long("acknowledge-sensitive")
                        .action(ArgAction::SetTrue)
                        .required(true)
                        .help("Acknowledge that this report is private provider evidence"),
                ),
        )
}

#[cfg(feature = "private-capture")]
fn init_private_capture_command() -> Command {
    use clap::ArgGroup;

    let reference = || {
        Arg::new("capture_ref")
            .value_name("CAPTURE_REF")
            .required(true)
    };

    Command::new("debug-capture")
        .about("Operate the encrypted private YouTube diagnostic vault")
        .long_about(
            "Operate the encrypted private YouTube diagnostic vault. Commands print only \
             bounded safe summaries except for the explicitly acknowledged, terminal-only \
             masked inspector. Private evidence cannot be redirected.",
        )
        .subcommand_required(true)
        .subcommand(Command::new("status").about("Show a safe vault availability summary"))
        .subcommand(Command::new("list").about("List retained short capture references"))
        .subcommand(
            Command::new("review")
                .about("Decrypt one capture and print only its safe review")
                .arg(reference()),
        )
        .subcommand(
            Command::new("replay")
                .about("Run a bounded replay without changing application playback")
                .arg(reference())
                .arg(
                    Arg::new("offline")
                        .long("offline")
                        .action(ArgAction::SetTrue)
                        .help("Replay the recorded provider response without network access"),
                )
                .arg(
                    Arg::new("fresh")
                        .long("fresh")
                        .action(ArgAction::SetTrue)
                        .requires("acknowledge_network")
                        .help("Make exactly one current provider request"),
                )
                .arg(
                    Arg::new("acknowledge_network")
                        .long("acknowledge-network")
                        .action(ArgAction::SetTrue)
                        .requires("fresh")
                        .help("Acknowledge that fresh replay makes one network request"),
                )
                .group(
                    ArgGroup::new("replay_mode")
                        .args(["offline", "fresh"])
                        .required(true)
                        .multiple(false),
                ),
        )
        .subcommand(
            Command::new("compare")
                .about("Compare one working and one failing encrypted capture")
                .arg(
                    Arg::new("working_ref")
                        .value_name("WORKING_REF")
                        .required(true),
                )
                .arg(
                    Arg::new("failing_ref")
                        .value_name("FAILING_REF")
                        .required(true),
                ),
        )
        .subcommand(
            Command::new("sanitize-preview")
                .about("Print the exact scanned diagnostic derivative without creating it")
                .arg(reference()),
        )
        .subcommand(
            Command::new("inspect")
                .about("Inspect one masked private capture in an interactive terminal")
                .long_about(
                    "Inspect one masked private capture in an interactive terminal. Credential \
                     values and signed URL query values remain masked. Output redirection is \
                     rejected.",
                )
                .arg(reference())
                .arg(
                    Arg::new("record")
                        .long("record")
                        .value_name("SEQUENCE")
                        .value_parser(value_parser!(u16))
                        .help("Inspect one record; omit to list the bounded record catalog"),
                )
                .arg(
                    Arg::new("acknowledge_sensitive")
                        .long("acknowledge-sensitive")
                        .action(ArgAction::SetTrue)
                        .required(true)
                        .help("Acknowledge that masked output still contains private evidence"),
                ),
        )
        .subcommand(
            Command::new("live")
                .about("Capture one UI-independent YouTube playback attempt")
                .long_about(
                    "Capture one UI-independent YouTube playback attempt in the encrypted private \
                     vault. The command requires an interactive terminal and prints only a safe \
                     reference; inspect the capture separately to view masked private evidence.",
                )
                .arg(
                    Arg::new("video_id")
                        .long("video-id")
                        .value_name("VIDEO_ID")
                        .required(true)
                        .help("YouTube video ID to inspect through the existing playback resolver"),
                )
                .arg(
                    Arg::new("acknowledge_sensitive")
                        .long("acknowledge-sensitive")
                        .action(ArgAction::SetTrue)
                        .required(true)
                        .help(
                            "Acknowledge that the encrypted capture may contain private evidence",
                        ),
                ),
        )
        .subcommand(
            Command::new("sanitize")
                .about("Create a separate allowlist-built diagnostic derivative")
                .arg(reference())
                .arg(
                    Arg::new("output")
                        .long("output")
                        .value_name("DIRECTORY")
                        .required(true)
                        .value_parser(value_parser!(std::path::PathBuf))
                        .help("New directory for the reviewed derivative"),
                ),
        )
        .subcommand(
            Command::new("open")
                .about("Open the encrypted private vault after acknowledging sensitivity")
                .arg(reference())
                .arg(
                    Arg::new("acknowledge_sensitive")
                        .long("acknowledge-sensitive")
                        .action(ArgAction::SetTrue)
                        .required(true)
                        .help("Acknowledge that the opened folder contains private evidence"),
                ),
        )
        .subcommand(
            Command::new("delete")
                .about("Permanently delete one encrypted private capture")
                .arg(reference())
                .arg(
                    Arg::new("acknowledge_delete")
                        .long("acknowledge-delete")
                        .action(ArgAction::SetTrue)
                        .required(true)
                        .help("Acknowledge permanent deletion of the selected capture"),
                ),
        )
        .subcommand(
            Command::new("purge-expired")
                .about("Remove expired captures under the bounded retention policy"),
        )
}

pub fn init_generate_command() -> Command {
    Command::new("generate")
        .about("Generate shell completion for the application CLI")
        .arg(
            Arg::new("shell")
                .action(ArgAction::Set)
                .value_parser(value_parser!(Shell))
                .required(true),
        )
}

pub fn init_playlist_subcommand() -> Command {
    Command::new("playlist")
        .about("Playlist editing")
        .subcommand_required(true)
        .subcommand(Command::new("new").about("Create a new playlist")
            .arg(Arg::new("name")
                .value_parser(clap::builder::NonEmptyStringValueParser::new()))
            .arg(Arg::new("description")
                .value_parser(clap::builder::NonEmptyStringValueParser::new())
                .required(false))
            .arg(Arg::new("public")
                .short('p')
                .long("public")
                .action(clap::ArgAction::SetTrue)
                .help("Sets the playlist to public"))
            .arg(Arg::new("collab")
                .short('c')
                .long("collab")
                .action(clap::ArgAction::SetTrue)
                .help("Sets the playlist to collaborative"))
            )
        .subcommand(Command::new("delete").about("Delete a playlist")
            .arg(Arg::new("id")
                .value_parser(clap::builder::NonEmptyStringValueParser::new())))
        .subcommand(Command::new("import").about("Imports all songs from a playlist into another playlist.")
            .arg(Arg::new("from")
                .value_parser(clap::builder::NonEmptyStringValueParser::new()))
            .arg(Arg::new("to")
                .value_parser(clap::builder::NonEmptyStringValueParser::new()))
            .arg(Arg::new("delete")
                .short('d')
                .long("delete")
                .action(clap::ArgAction::SetTrue)
                .help("Deletes any previously imported tracks that are no longer in the imported playlist since last import."))
            .after_help("Import data for each playlist is stored inside the application's cache folder. If imported again, the command only imports new tracks since last import."))
        .subcommand(Command::new("list").about("Lists all user playlists."))
        .subcommand(Command::new("fork").about("Creates a copy of a playlist and imports it.")
            .arg(Arg::new("id")
                .value_parser(clap::builder::NonEmptyStringValueParser::new())))
        .subcommand(Command::new("sync").about("Syncs imports for all playlists or a single playlist.")
            .arg(Arg::new("id")
                .required(false)
                .value_parser(clap::builder::NonEmptyStringValueParser::new()))
            .arg(Arg::new("delete")
                .short('d')
                .long("delete")
                .action(clap::ArgAction::SetTrue)
                .help("Deletes any previously imported tracks that are no longer in an imported playlist since last import.")))
        .subcommand(Command::new("edit").about("Add tracks or remove all occurrences of tracks or albums from a playlist.")
            .arg(Arg::new("action")
                .help("Action to perform")
                .required(true)
                .value_parser(EnumValueParser::<EditAction>::new()))
            .arg(Arg::new("playlist_id")
                .help("Playlist ID")
                .required(true)
                .value_parser(clap::builder::NonEmptyStringValueParser::new()))
            .arg(Arg::new("track_id")
                .long("track-id")
                .short('t')
                .help("Track ID to add, or remove all occurrences of")
                .value_parser(clap::builder::NonEmptyStringValueParser::new()))
            .arg(Arg::new("album_id")
                .long("album-id")
                .short('a')
                .help("Album ID to add, or remove all occurrences of")
                .value_parser(clap::builder::NonEmptyStringValueParser::new()))
            .group(
                ArgGroup::new("content_id")
                    .args(["track_id", "album_id"])
                    .required(true)
            ))
}

pub fn init_print_features_command() -> Command {
    Command::new("features").about("Print compiled in features")
}

pub fn init_diagnostics_command() -> Command {
    Command::new("diagnostics")
        .about("Print a credential-safe support and runtime report")
        .arg(
            Arg::new("live")
                .long("live")
                .action(ArgAction::SetTrue)
                .help("Read provider and playback state from a running application"),
        )
        .arg(
            Arg::new("bundle")
                .long("bundle")
                .value_name("FOLDER")
                .value_parser(value_parser!(std::path::PathBuf))
                .conflicts_with_all(["live", "review_bundle"])
                .help("Create an allowlisted local support bundle for review"),
        )
        .arg(
            Arg::new("review_bundle")
                .long("review-bundle")
                .value_name("FOLDER")
                .value_parser(value_parser!(std::path::PathBuf))
                .conflicts_with_all(["live", "bundle"])
                .help("Verify checksums and rescan a support bundle before sharing"),
        )
        .arg(
            Arg::new("verbose_seconds")
                .long("verbose-seconds")
                .value_name("SECONDS")
                .value_parser(value_parser!(u16).range(1..=300))
                .requires("live")
                .conflicts_with_all(["bundle", "review_bundle", "trend"])
                .help("Temporarily enable trace-level local diagnostics"),
        )
        .arg(
            Arg::new("trend")
                .long("trend")
                .action(ArgAction::SetTrue)
                .conflicts_with_all(["live", "bundle", "review_bundle", "verbose_seconds"])
                .help("Compare recent retained local timing samples"),
        )
}

pub fn init_unified_command() -> Command {
    Command::new("unified")
        .about("Manage local provider-neutral playlists")
        .subcommand_required(true)
        .subcommand(Command::new("list").about("List local unified playlists"))
        .subcommand(
            Command::new("new")
                .about("Create an empty local unified playlist")
                .arg(Arg::new("name").required(true)),
        )
        .subcommand(
            Command::new("delete")
                .about("Delete a local unified playlist")
                .arg(Arg::new("id").required(true)),
        )
        .subcommand(
            Command::new("add")
                .about("Add a provider item to a local unified playlist")
                .arg(Arg::new("playlist_id").long("playlist-id").required(true))
                .arg(
                    Arg::new("provider")
                        .long("provider")
                        .value_parser(["spotify", "youtube"])
                        .required(true),
                )
                .arg(Arg::new("item_id").long("item-id").required(true))
                .arg(Arg::new("title").long("title").required(true))
                .arg(Arg::new("artists").long("artists").default_value(""))
                .arg(
                    Arg::new("duration_ms")
                        .long("duration-ms")
                        .value_parser(value_parser!(u64)),
                )
                .arg(Arg::new("video").long("video").action(ArgAction::SetTrue)),
        )
        .subcommand(
            Command::new("remove")
                .about("Remove one exact occurrence, or explicitly remove all matching occurrences")
                .arg(Arg::new("playlist_id").long("playlist-id").required(true))
                .arg(Arg::new("item_id").long("item-id"))
                .arg(
                    Arg::new("entry_id")
                        .long("entry-id")
                        .value_parser(value_parser!(u64))
                        .help("Exact local occurrence ID to remove"),
                )
                .arg(
                    Arg::new("all_occurrences")
                        .long("all-occurrences")
                        .action(ArgAction::SetTrue)
                        .help("Explicitly remove all occurrences matching --item-id"),
                ),
        )
        .subcommand(
            Command::new("export")
                .about("Export local unified playlists as versioned JSON or JSPF")
                .arg(Arg::new("output").long("output").required(true))
                .arg(
                    Arg::new("format")
                        .long("format")
                        .value_parser(["json", "jspf"])
                        .default_value("json"),
                )
                .arg(Arg::new("playlist_id").long("playlist-id")),
        )
        .subcommand(
            Command::new("link")
                .about("Link a unified playlist to provider-native playlists")
                .arg(Arg::new("playlist_id").long("playlist-id").required(true))
                .arg(Arg::new("spotify_id").long("spotify-id"))
                .arg(Arg::new("youtube_id").long("youtube-id"))
                .arg(Arg::new("listenbrainz_id").long("listenbrainz-id"))
                .group(
                    ArgGroup::new("projection")
                        .args(["spotify_id", "youtube_id", "listenbrainz_id"])
                        .required(true),
                ),
        )
        .subcommand(
            Command::new("project")
                .about("Preview or apply a unified playlist projection")
                .arg(Arg::new("playlist_id").long("playlist-id").required(true))
                .arg(
                    Arg::new("provider")
                        .long("provider")
                        .value_parser(["youtube"])
                        .required(true),
                )
                .arg(
                    Arg::new("target_playlist_id")
                        .long("target-playlist-id")
                        .required(true),
                )
                .arg(
                    Arg::new("check_remote")
                        .long("check-remote")
                        .action(ArgAction::SetTrue)
                        .help("Fetch the target playlist and report remote-side changes"),
                )
                .arg(
                    Arg::new("force")
                        .long("force")
                        .action(ArgAction::SetTrue)
                        .help("Allow --apply despite recorded local or remote snapshot conflicts"),
                )
                .arg(
                    Arg::new("resolution")
                        .long("resolution")
                        .value_parser(["local", "merge", "match"])
                        .help("Resolve conflicts locally, merge remote items, or match Spotify metadata to YouTube"),
                )
                .arg(Arg::new("apply").long("apply").action(ArgAction::SetTrue)),
        )
}

pub fn init_listenbrainz_command() -> Command {
    Command::new("listenbrainz")
        .about("Use ListenBrainz as an optional unified-playlist backup")
        .subcommand_required(true)
        .subcommand(Command::new("auth").about("Show token setup instructions"))
        .subcommand(Command::new("status").about("Show token configuration status"))
        .subcommand(
            Command::new("probe")
                .about("Measure ListenBrainz capabilities without remote writes")
                .arg(
                    Arg::new("spotify_artist_id")
                        .long("spotify-artist-id")
                        .value_name("ID"),
                )
                .arg(Arg::new("unified_id").long("unified-id").value_name("ID"))
                .arg(
                    Arg::new("sample_limit")
                        .long("sample-limit")
                        .value_parser(value_parser!(usize))
                        .default_value("100"),
                )
                .arg(
                    Arg::new("description_budget")
                        .long("description-budget")
                        .value_parser(value_parser!(usize))
                        .default_value("9000"),
                )
                .arg(Arg::new("json").long("json").action(ArgAction::SetTrue)),
        )
        .subcommand(
            Command::new("backup")
                .about("Create a remote ListenBrainz backup of a local playlist")
                .arg(Arg::new("playlist_id").long("playlist-id").required(true)),
        )
        .subcommand(
            Command::new("restore")
                .about("Preview or restore a ListenBrainz playlist locally")
                .arg(Arg::new("playlist_id").long("playlist-id").required(true))
                .arg(Arg::new("unified_id").long("unified-id"))
                .arg(Arg::new("apply").long("apply").action(ArgAction::SetTrue)),
        )
        .subcommand(
            Command::new("diff")
                .about("Compare a remote ListenBrainz playlist with a local playlist")
                .arg(Arg::new("playlist_id").long("playlist-id").required(true))
                .arg(Arg::new("unified_id").long("unified-id").required(true)),
        )
        .subcommand(
            Command::new("plan")
                .about("Generate a manifest-first, read-only three-way sync plan")
                .arg(Arg::new("playlist_id").long("playlist-id").required(true))
                .arg(Arg::new("unified_id").long("unified-id").required(true))
                .arg(
                    Arg::new("expected_remote_fingerprint")
                        .long("expected-remote-fingerprint")
                        .value_name("SHA256")
                        .help(
                            "Fail closed unless the current normalized remote fingerprint matches",
                        ),
                )
                .arg(
                    Arg::new("initialize_base")
                        .long("initialize-base")
                        .action(ArgAction::SetTrue)
                        .help("Persist this verified remote manifest as the initial sync base"),
                )
                .arg(
                    Arg::new("format")
                        .long("format")
                        .value_parser(["text", "json"])
                        .default_value("text"),
                ),
        )
        .subcommand(
            Command::new("projection-preview")
                .about("Preview a redacted native JSPF projection without remote writes")
                .arg(Arg::new("unified_id").long("unified-id").required(true))
                .arg(
                    Arg::new("recording_relation")
                        .long("recording-relation")
                        .value_name("PROVIDER:KIND:ID=MBID")
                        .action(ArgAction::Append)
                        .help("Attach an explicit MusicBrainz recording relation"),
                )
                .arg(
                    Arg::new("description_budget")
                        .long("description-budget")
                        .value_parser(value_parser!(usize))
                        .default_value("9000"),
                )
                .arg(
                    Arg::new("format")
                        .long("format")
                        .value_parser(["text", "json"])
                        .default_value("text"),
                ),
        )
        .subcommand(
            Command::new("push")
                .about("Preview or explicitly apply a native ListenBrainz projection")
                .arg(Arg::new("playlist_id").long("playlist-id").required(true))
                .arg(Arg::new("unified_id").long("unified-id").required(true))
                .arg(
                    Arg::new("recording_relation")
                        .long("recording-relation")
                        .value_name("PROVIDER:KIND:ID=MBID")
                        .action(ArgAction::Append)
                        .help("Attach an explicit MusicBrainz recording relation"),
                )
                .arg(
                    Arg::new("operation_id")
                        .long("operation-id")
                        .value_name("ID")
                        .help("Stable caller-provided identity required with --apply"),
                )
                .arg(Arg::new("apply").long("apply").action(ArgAction::SetTrue))
                .arg(
                    Arg::new("format")
                        .long("format")
                        .value_parser(["text", "json"])
                        .default_value("text"),
                ),
        )
        .subcommand(
            Command::new("recover")
                .about("Read back a pending push outcome without retrying it")
                .arg(Arg::new("playlist_id").long("playlist-id").required(true))
                .arg(Arg::new("unified_id").long("unified-id").required(true))
                .arg(
                    Arg::new("format")
                        .long("format")
                        .value_parser(["text", "json"])
                        .default_value("text"),
                ),
        )
        .subcommand(
            Command::new("pull-preview")
                .about("Preview remote changes against the persisted sync base")
                .arg(Arg::new("playlist_id").long("playlist-id").required(true))
                .arg(Arg::new("unified_id").long("unified-id").required(true))
                .arg(
                    Arg::new("format")
                        .long("format")
                        .value_parser(["text", "json"])
                        .default_value("text"),
                ),
        )
        .subcommand(
            Command::new("pull")
                .about("Preview or explicitly apply safe remote changes locally")
                .arg(Arg::new("playlist_id").long("playlist-id").required(true))
                .arg(Arg::new("unified_id").long("unified-id").required(true))
                .arg(
                    Arg::new("operation_id")
                        .long("operation-id")
                        .value_name("ID")
                        .help("Stable caller-provided identity required with --apply"),
                )
                .arg(Arg::new("apply").long("apply").action(ArgAction::SetTrue))
                .arg(
                    Arg::new("format")
                        .long("format")
                        .value_parser(["text", "json"])
                        .default_value("text"),
                ),
        )
        .subcommand(
            Command::new("pull-rollback")
                .about("Restore the last persisted local pre-apply snapshot")
                .arg(Arg::new("playlist_id").long("playlist-id").required(true))
                .arg(Arg::new("unified_id").long("unified-id").required(true))
                .arg(
                    Arg::new("format")
                        .long("format")
                        .value_parser(["text", "json"])
                        .default_value("text"),
                ),
        )
        .subcommand(
            Command::new("resolve")
                .about("Preview or explicitly apply a ListenBrainz conflict policy")
                .arg(Arg::new("playlist_id").long("playlist-id").required(true))
                .arg(Arg::new("unified_id").long("unified-id").required(true))
                .arg(
                    Arg::new("policy")
                        .long("policy")
                        .value_parser(["keep-local", "keep-listenbrainz", "merge"])
                        .required(true),
                )
                .arg(
                    Arg::new("decision")
                        .long("decision")
                        .value_name("INDEX=local|listenbrainz")
                        .action(ArgAction::Append)
                        .help("Choose one side for an indexed merge conflict"),
                )
                .arg(
                    Arg::new("import_unlinked")
                        .long("import-unlinked")
                        .value_name("INDEX=PROVIDER:KIND:ID=MBID")
                        .action(ArgAction::Append)
                        .help("Explicitly map one unlinked native row for merge import"),
                )
                .arg(
                    Arg::new("recording_relation")
                        .long("recording-relation")
                        .value_name("PROVIDER:KIND:ID=MBID")
                        .action(ArgAction::Append)
                        .help("Attach an explicit MusicBrainz recording relation"),
                )
                .arg(
                    Arg::new("operation_id")
                        .long("operation-id")
                        .value_name("ID")
                        .help("Stable caller-provided identity required with --apply"),
                )
                .arg(Arg::new("apply").long("apply").action(ArgAction::SetTrue))
                .arg(
                    Arg::new("format")
                        .long("format")
                        .value_parser(["text", "json"])
                        .default_value("text"),
                ),
        )
}

pub fn init_lyrics_command() -> Command {
    add_id_or_name_group_optional(
        Command::new("lyrics").about(
            "Print provided track's lyrics or current playing track, if no argument specified",
        ),
        false,
    )
}

#[cfg(test)]
mod tests {
    use super::{
        init_demo_command, init_diagnostics_command, init_listenbrainz_command,
        init_unified_command, init_youtube_subcommand,
    };

    #[test]
    fn welcome_demo_has_bounded_offline_scenarios_and_dimensions() {
        let matches = init_demo_command()
            .try_get_matches_from([
                "demo",
                "welcome",
                "--scenario",
                "youtube-oauth-ready",
                "--step",
                "youtube",
                "--width",
                "120",
                "--height",
                "30",
                "--interactive",
            ])
            .unwrap();
        let (_, welcome) = matches.subcommand().unwrap();
        assert_eq!(
            welcome.get_one::<String>("scenario").map(String::as_str),
            Some("youtube-oauth-ready")
        );
        assert_eq!(
            welcome.get_one::<String>("step").map(String::as_str),
            Some("youtube")
        );
        assert_eq!(welcome.get_one::<u16>("width"), Some(&120));
        assert_eq!(welcome.get_one::<u16>("height"), Some(&30));
        assert!(welcome.get_flag("interactive"));

        assert!(init_demo_command()
            .try_get_matches_from(["demo", "welcome", "--scenario", "live-account"])
            .is_err());
        assert!(init_demo_command()
            .try_get_matches_from(["demo", "welcome", "--width", "20"])
            .is_err());
        assert!(init_demo_command()
            .try_get_matches_from(["demo", "welcome", "--step", "accounts"])
            .is_err());
    }

    #[test]
    fn parses_listenbrainz_capability_probe() {
        let matches = init_listenbrainz_command()
            .try_get_matches_from([
                "listenbrainz",
                "probe",
                "--spotify-artist-id",
                "artist123",
                "--unified-id",
                "playlist123",
                "--sample-limit",
                "25",
                "--description-budget",
                "9000",
                "--json",
            ])
            .unwrap();
        let (_, probe) = matches.subcommand().unwrap();

        assert_eq!(
            probe.get_one::<String>("spotify_artist_id").unwrap(),
            "artist123"
        );
        assert_eq!(
            probe.get_one::<String>("unified_id").unwrap(),
            "playlist123"
        );
        assert_eq!(*probe.get_one::<usize>("sample_limit").unwrap(), 25);
        assert_eq!(*probe.get_one::<usize>("description_budget").unwrap(), 9000);
        assert!(probe.get_flag("json"));
    }

    #[test]
    fn parses_listenbrainz_three_way_plan_stale_guard() {
        let fingerprint = "a".repeat(64);
        let matches = init_listenbrainz_command()
            .try_get_matches_from([
                "listenbrainz",
                "plan",
                "--playlist-id",
                "remote",
                "--unified-id",
                "local",
                "--expected-remote-fingerprint",
                &fingerprint,
                "--format",
                "json",
            ])
            .unwrap();
        let (_, plan) = matches.subcommand().unwrap();

        assert_eq!(plan.get_one::<String>("playlist_id").unwrap(), "remote");
        assert_eq!(plan.get_one::<String>("unified_id").unwrap(), "local");
        assert_eq!(
            plan.get_one::<String>("expected_remote_fingerprint")
                .unwrap(),
            &fingerprint
        );
        assert_eq!(plan.get_one::<String>("format").unwrap(), "json");
    }

    #[test]
    fn parses_listenbrainz_native_projection_preview_relations() {
        let matches = init_listenbrainz_command()
            .try_get_matches_from([
                "listenbrainz",
                "projection-preview",
                "--unified-id",
                "local",
                "--recording-relation",
                "spotify:track:track-1=12345678-1234-1234-1234-123456789abc",
                "--recording-relation",
                "youtube-music:video:video-1=abcdefab-cdef-abcd-efab-cdefabcdefab",
                "--format",
                "json",
            ])
            .unwrap();
        let (_, preview) = matches.subcommand().unwrap();

        assert_eq!(preview.get_one::<String>("unified_id").unwrap(), "local");
        assert_eq!(
            preview
                .get_many::<String>("recording_relation")
                .unwrap()
                .count(),
            2
        );
        assert_eq!(preview.get_one::<String>("format").unwrap(), "json");
    }

    #[test]
    fn parses_explicit_listenbrainz_push_apply_and_recovery() {
        let matches = init_listenbrainz_command()
            .try_get_matches_from([
                "listenbrainz",
                "push",
                "--playlist-id",
                "remote",
                "--unified-id",
                "local",
                "--operation-id",
                "operation-1",
                "--apply",
                "--format",
                "json",
            ])
            .unwrap();
        let (_, push) = matches.subcommand().unwrap();
        assert!(push.get_flag("apply"));
        assert_eq!(
            push.get_one::<String>("operation_id").unwrap(),
            "operation-1"
        );

        let matches = init_listenbrainz_command()
            .try_get_matches_from([
                "listenbrainz",
                "recover",
                "--playlist-id",
                "remote",
                "--unified-id",
                "local",
            ])
            .unwrap();
        assert_eq!(matches.subcommand().unwrap().0, "recover");

        let matches = init_listenbrainz_command()
            .try_get_matches_from([
                "listenbrainz",
                "plan",
                "--playlist-id",
                "remote",
                "--unified-id",
                "local",
                "--initialize-base",
            ])
            .unwrap();
        assert!(matches.subcommand().unwrap().1.get_flag("initialize_base"));
    }

    #[test]
    fn parses_read_only_listenbrainz_pull_preview() {
        let matches = init_listenbrainz_command()
            .try_get_matches_from([
                "listenbrainz",
                "pull-preview",
                "--playlist-id",
                "remote",
                "--unified-id",
                "local",
                "--format",
                "json",
            ])
            .unwrap();
        let (_, preview) = matches.subcommand().unwrap();
        assert_eq!(preview.get_one::<String>("playlist_id").unwrap(), "remote");
        assert_eq!(preview.get_one::<String>("unified_id").unwrap(), "local");
        assert_eq!(preview.get_one::<String>("format").unwrap(), "json");
    }

    #[test]
    fn parses_explicit_listenbrainz_pull_apply_and_rollback() {
        let matches = init_listenbrainz_command()
            .try_get_matches_from([
                "listenbrainz",
                "pull",
                "--playlist-id",
                "remote",
                "--unified-id",
                "local",
                "--operation-id",
                "pull-1",
                "--apply",
            ])
            .unwrap();
        let (_, pull) = matches.subcommand().unwrap();
        assert!(pull.get_flag("apply"));
        assert_eq!(pull.get_one::<String>("operation_id").unwrap(), "pull-1");

        let matches = init_listenbrainz_command()
            .try_get_matches_from([
                "listenbrainz",
                "pull-rollback",
                "--playlist-id",
                "remote",
                "--unified-id",
                "local",
            ])
            .unwrap();
        assert_eq!(matches.subcommand().unwrap().0, "pull-rollback");
    }

    #[test]
    fn parses_only_supported_listenbrainz_resolution_policies_and_decisions() {
        for policy in ["keep-local", "keep-listenbrainz", "merge"] {
            let matches = init_listenbrainz_command()
                .try_get_matches_from([
                    "listenbrainz",
                    "resolve",
                    "--playlist-id",
                    "remote",
                    "--unified-id",
                    "local",
                    "--policy",
                    policy,
                    "--decision",
                    "0=local",
                ])
                .unwrap();
            let (_, resolve) = matches.subcommand().unwrap();
            assert_eq!(resolve.get_one::<String>("policy").unwrap(), policy);
        }
        assert!(init_listenbrainz_command()
            .try_get_matches_from([
                "listenbrainz",
                "resolve",
                "--playlist-id",
                "remote",
                "--unified-id",
                "local",
                "--policy",
                "automatic",
            ])
            .is_err());
    }

    #[test]
    fn parses_youtube_playlist_mutation_commands() {
        let matches = init_youtube_subcommand()
            .try_get_matches_from([
                "youtube",
                "playlist",
                "add",
                "--playlist-id",
                "PL123",
                "--video-id",
                "video123",
            ])
            .unwrap();
        let (_, playlist) = matches.subcommand().unwrap();
        let (_, add) = playlist.subcommand().unwrap();
        assert_eq!(add.get_one::<String>("playlist_id").unwrap(), "PL123");
        assert_eq!(add.get_one::<String>("video_id").unwrap(), "video123");
    }

    #[test]
    fn parses_youtube_device_login_command() {
        let matches = init_youtube_subcommand()
            .try_get_matches_from([
                "youtube",
                "login",
                "--client-id",
                "client-id",
                "--client-secret",
                "client-secret",
                "--no-open",
            ])
            .unwrap();
        let (_, login) = matches.subcommand().unwrap();
        assert_eq!(login.get_one::<String>("client_id").unwrap(), "client-id");
        assert_eq!(
            login.get_one::<String>("client_secret").unwrap(),
            "client-secret"
        );
        assert!(login.get_flag("no_open"));
    }

    #[test]
    fn parses_youtube_dedicated_browser_login_command() {
        let matches = init_youtube_subcommand()
            .try_get_matches_from([
                "youtube",
                "browser-login",
                "--browser",
                "browser.exe",
                "--keep-open",
            ])
            .unwrap();
        let (_, login) = matches.subcommand().unwrap();
        assert_eq!(
            login.get_one::<std::path::PathBuf>("browser").unwrap(),
            &std::path::PathBuf::from("browser.exe")
        );
        assert!(login.get_flag("keep_open"));
    }

    #[test]
    fn parses_a_specific_youtube_playback_auth_probe() {
        let matches = init_youtube_subcommand()
            .try_get_matches_from([
                "youtube",
                "status",
                "--check",
                "--video-id",
                "video123",
                "--audio-output",
            ])
            .unwrap();
        let (_, status) = matches.subcommand().unwrap();
        assert!(status.get_flag("check"));
        assert!(status.get_flag("audio_output"));
        assert_eq!(status.get_one::<String>("video_id").unwrap(), "video123");
    }

    #[test]
    fn youtube_probe_is_native_only_unless_browser_fallback_is_explicit() {
        let matches = init_youtube_subcommand()
            .try_get_matches_from(["youtube", "probe", "--video-id", "video123", "--json"])
            .unwrap();
        let (_, probe) = matches.subcommand().unwrap();
        assert_eq!(probe.get_one::<String>("video_id").unwrap(), "video123");
        assert_eq!(probe.get_one::<String>("client").unwrap(), "auto");
        assert_eq!(probe.get_one::<String>("decoder_chunk_size").unwrap(), "1m");
        assert!(probe.get_flag("json"));
        assert!(!probe.get_flag("allow_browser_fallback"));

        let matches = init_youtube_subcommand()
            .try_get_matches_from([
                "youtube",
                "probe",
                "--video-id",
                "video123",
                "--client",
                "web-remix",
                "--decoder-chunk-size",
                "10m",
                "--allow-browser-fallback",
            ])
            .unwrap();
        let (_, probe) = matches.subcommand().unwrap();
        assert_eq!(probe.get_one::<String>("client").unwrap(), "web-remix");
        assert_eq!(
            probe.get_one::<String>("decoder_chunk_size").unwrap(),
            "10m"
        );
        assert!(probe.get_flag("allow_browser_fallback"));

        let matches = init_youtube_subcommand()
            .try_get_matches_from([
                "youtube",
                "probe",
                "--video-id",
                "video123",
                "--client",
                "visionos",
            ])
            .unwrap();
        let (_, probe) = matches.subcommand().unwrap();
        assert_eq!(probe.get_one::<String>("client").unwrap(), "visionos");
        assert!(!probe.get_flag("allow_browser_fallback"));
    }

    #[test]
    fn parses_a_specific_youtube_transport_diagnostic() {
        let matches = init_youtube_subcommand()
            .try_get_matches_from([
                "youtube",
                "status",
                "--check",
                "--video-id",
                "video123",
                "--transport-diagnostic",
            ])
            .unwrap();
        let (_, status) = matches.subcommand().unwrap();
        assert!(status.get_flag("check"));
        assert!(status.get_flag("transport_diagnostic"));
        assert_eq!(status.get_one::<String>("video_id").unwrap(), "video123");
    }

    #[cfg(feature = "private-capture")]
    #[test]
    fn parses_private_youtube_developer_inspection() {
        let matches = init_youtube_subcommand()
            .try_get_matches_from([
                "youtube",
                "debug",
                "inspect",
                "--video-id",
                "video123",
                "--transport",
                "--json",
                "--acknowledge-sensitive",
            ])
            .unwrap();
        let (_, debug) = matches.subcommand().unwrap();
        let (_, inspect) = debug.subcommand().unwrap();
        assert_eq!(inspect.get_one::<String>("video_id").unwrap(), "video123");
        assert!(inspect.get_flag("transport"));
        assert!(inspect.get_flag("json"));
        assert!(inspect.get_flag("acknowledge_sensitive"));
    }

    #[cfg(feature = "private-capture")]
    #[test]
    fn parses_private_youtube_developer_comparison() {
        let matches = init_youtube_subcommand()
            .try_get_matches_from([
                "youtube",
                "debug",
                "compare",
                "--config-a",
                "account-a/config",
                "--cache-a",
                "account-a/cache",
                "--config-b",
                "account-b/config",
                "--cache-b",
                "account-b/cache",
                "--video-id",
                "video123",
                "--acknowledge-sensitive",
            ])
            .unwrap();
        let (_, debug) = matches.subcommand().unwrap();
        let (_, compare) = debug.subcommand().unwrap();
        assert_eq!(
            compare.get_one::<std::path::PathBuf>("config_a").unwrap(),
            &std::path::PathBuf::from("account-a/config")
        );
        assert_eq!(
            compare.get_one::<std::path::PathBuf>("cache_b").unwrap(),
            &std::path::PathBuf::from("account-b/cache")
        );
    }

    #[cfg(feature = "private-capture")]
    #[test]
    fn private_youtube_developer_actions_require_sensitive_acknowledgement() {
        assert!(init_youtube_subcommand()
            .try_get_matches_from(["youtube", "debug", "inspect", "--video-id", "video123",])
            .is_err());
        assert!(init_youtube_subcommand()
            .try_get_matches_from([
                "youtube",
                "debug",
                "compare",
                "--config-a",
                "account-a/config",
                "--cache-a",
                "account-a/cache",
                "--config-b",
                "account-b/config",
                "--cache-b",
                "account-b/cache",
                "--video-id",
                "video123",
            ])
            .is_err());
    }

    #[cfg(not(feature = "private-capture"))]
    #[test]
    fn private_capture_cli_is_absent_without_the_feature() {
        assert!(init_youtube_subcommand()
            .try_get_matches_from(["youtube", "debug-capture", "status"])
            .is_err());
    }

    #[cfg(feature = "private-capture")]
    #[test]
    fn parses_private_capture_safe_references_and_output_path() {
        let preview = init_youtube_subcommand()
            .try_get_matches_from(["youtube", "debug-capture", "sanitize-preview", "0123abcd"])
            .unwrap();
        let (_, capture) = preview.subcommand().unwrap();
        let (_, preview) = capture.subcommand().unwrap();
        assert_eq!(
            preview.get_one::<String>("capture_ref").map(String::as_str),
            Some("0123abcd")
        );

        let matches = init_youtube_subcommand()
            .try_get_matches_from([
                "youtube",
                "debug-capture",
                "sanitize",
                "0123abcd",
                "--output",
                "diagnostic-output",
            ])
            .unwrap();
        let (_, capture) = matches.subcommand().unwrap();
        let (_, sanitize) = capture.subcommand().unwrap();
        assert_eq!(
            sanitize
                .get_one::<String>("capture_ref")
                .map(String::as_str),
            Some("0123abcd")
        );
        assert_eq!(
            sanitize.get_one::<std::path::PathBuf>("output").unwrap(),
            &std::path::PathBuf::from("diagnostic-output")
        );
    }

    #[cfg(feature = "private-capture")]
    #[test]
    fn private_capture_reference_values_do_not_use_an_echoing_value_parser() {
        for rejected in [
            "0123ABCd",
            "0123abc",
            "0123abcde",
            "0123abcg",
            "../0123abcd",
        ] {
            let matches = init_youtube_subcommand()
                .try_get_matches_from(["youtube", "debug-capture", "review", rejected])
                .unwrap();
            let (_, capture) = matches.subcommand().unwrap();
            let (_, review) = capture.subcommand().unwrap();
            assert_eq!(
                review.get_one::<String>("capture_ref").map(String::as_str),
                Some(rejected)
            );
        }
    }

    #[cfg(feature = "private-capture")]
    #[test]
    fn private_capture_replay_requires_exactly_one_mode() {
        for rejected in [
            vec!["youtube", "debug-capture", "replay", "0123abcd"],
            vec![
                "youtube",
                "debug-capture",
                "replay",
                "0123abcd",
                "--offline",
                "--fresh",
                "--acknowledge-network",
            ],
        ] {
            assert!(init_youtube_subcommand()
                .try_get_matches_from(rejected)
                .is_err());
        }
        assert!(init_youtube_subcommand()
            .try_get_matches_from([
                "youtube",
                "debug-capture",
                "replay",
                "0123abcd",
                "--offline",
            ])
            .is_ok());
    }

    #[cfg(feature = "private-capture")]
    #[test]
    fn private_capture_fresh_replay_requires_network_acknowledgement() {
        assert!(init_youtube_subcommand()
            .try_get_matches_from(["youtube", "debug-capture", "replay", "0123abcd", "--fresh",])
            .is_err());
        assert!(init_youtube_subcommand()
            .try_get_matches_from([
                "youtube",
                "debug-capture",
                "replay",
                "0123abcd",
                "--fresh",
                "--acknowledge-network",
            ])
            .is_ok());
    }

    #[cfg(feature = "private-capture")]
    #[test]
    fn private_capture_sensitive_and_destructive_actions_require_acknowledgement() {
        for (action, acknowledgement) in [
            ("open", "--acknowledge-sensitive"),
            ("inspect", "--acknowledge-sensitive"),
            ("delete", "--acknowledge-delete"),
        ] {
            assert!(init_youtube_subcommand()
                .try_get_matches_from(["youtube", "debug-capture", action, "0123abcd"])
                .is_err());
            assert!(init_youtube_subcommand()
                .try_get_matches_from([
                    "youtube",
                    "debug-capture",
                    action,
                    "0123abcd",
                    acknowledgement,
                ])
                .is_ok());
        }
    }

    #[cfg(feature = "private-capture")]
    #[test]
    fn private_live_capture_requires_sensitive_acknowledgement() {
        assert!(init_youtube_subcommand()
            .try_get_matches_from(["youtube", "debug-capture", "live", "--video-id", "video123",])
            .is_err());
        let matches = init_youtube_subcommand()
            .try_get_matches_from([
                "youtube",
                "debug-capture",
                "live",
                "--video-id",
                "video123",
                "--acknowledge-sensitive",
            ])
            .unwrap();
        let (_, capture) = matches.subcommand().unwrap();
        let (_, live) = capture.subcommand().unwrap();
        assert_eq!(
            live.get_one::<String>("video_id").map(String::as_str),
            Some("video123")
        );
        assert!(live.get_flag("acknowledge_sensitive"));
    }

    #[cfg(feature = "private-capture")]
    #[test]
    fn private_capture_inspection_accepts_only_a_u16_record_sequence() {
        let matches = init_youtube_subcommand()
            .try_get_matches_from([
                "youtube",
                "debug-capture",
                "inspect",
                "0123abcd",
                "--record",
                "42",
                "--acknowledge-sensitive",
            ])
            .unwrap();
        let (_, capture) = matches.subcommand().unwrap();
        let (_, inspect) = capture.subcommand().unwrap();
        assert_eq!(inspect.get_one::<u16>("record"), Some(&42));

        for rejected in ["-1", "65536", "not-a-sequence"] {
            assert!(init_youtube_subcommand()
                .try_get_matches_from([
                    "youtube",
                    "debug-capture",
                    "inspect",
                    "0123abcd",
                    "--record",
                    rejected,
                    "--acknowledge-sensitive",
                ])
                .is_err());
        }
    }

    #[test]
    fn parses_live_sanitized_diagnostics() {
        let matches = init_diagnostics_command()
            .try_get_matches_from(["diagnostics", "--live"])
            .unwrap();
        assert!(matches.get_flag("live"));
    }

    #[test]
    fn parses_support_bundle_create_and_review_actions() {
        let create = init_diagnostics_command()
            .try_get_matches_from(["diagnostics", "--bundle", "support-output"])
            .unwrap();
        assert_eq!(
            create.get_one::<std::path::PathBuf>("bundle").unwrap(),
            &std::path::PathBuf::from("support-output")
        );
        let review = init_diagnostics_command()
            .try_get_matches_from(["diagnostics", "--review-bundle", "support-output"])
            .unwrap();
        assert!(review
            .get_one::<std::path::PathBuf>("review_bundle")
            .is_some());
        assert!(init_diagnostics_command()
            .try_get_matches_from(["diagnostics", "--live", "--bundle", "support-output"])
            .is_err());
        let verbose = init_diagnostics_command()
            .try_get_matches_from(["diagnostics", "--live", "--verbose-seconds", "30"])
            .unwrap();
        assert_eq!(verbose.get_one::<u16>("verbose_seconds"), Some(&30));
    }

    #[test]
    fn parses_unified_projection_as_dry_run_by_default() {
        let matches = init_unified_command()
            .try_get_matches_from([
                "unified",
                "project",
                "--playlist-id",
                "local-1",
                "--provider",
                "youtube",
                "--target-playlist-id",
                "PL123",
            ])
            .unwrap();
        let (_, project) = matches.subcommand().unwrap();
        assert!(!project.get_flag("apply"));
        assert!(!project.get_flag("force"));
        assert!(project.get_one::<String>("resolution").is_none());
        assert_eq!(project.get_one::<String>("provider").unwrap(), "youtube");

        let match_resolution = init_unified_command()
            .try_get_matches_from([
                "unified",
                "project",
                "--playlist-id",
                "local-1",
                "--provider",
                "youtube",
                "--target-playlist-id",
                "PL123",
                "--resolution",
                "match",
            ])
            .unwrap();
        let (_, project) = match_resolution.subcommand().unwrap();
        assert_eq!(project.get_one::<String>("resolution").unwrap(), "match");
    }

    #[test]
    fn unified_remove_exposes_exact_occurrence_and_explicit_remove_all_modes() {
        let exact = init_unified_command()
            .try_get_matches_from([
                "unified",
                "remove",
                "--playlist-id",
                "local-1",
                "--entry-id",
                "7",
            ])
            .unwrap();
        let (_, exact) = exact.subcommand().unwrap();
        assert_eq!(exact.get_one::<u64>("entry_id"), Some(&7));
        assert!(!exact.get_flag("all_occurrences"));

        let all = init_unified_command()
            .try_get_matches_from([
                "unified",
                "remove",
                "--playlist-id",
                "local-1",
                "--item-id",
                "track-1",
                "--all-occurrences",
            ])
            .unwrap();
        let (_, all) = all.subcommand().unwrap();
        assert!(all.get_flag("all_occurrences"));
    }
}
