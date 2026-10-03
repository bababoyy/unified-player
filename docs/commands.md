# Commands and key bindings

Every key binding, every action, and the command-line interface. To change a
binding, see [key maps](config.md#keymaps).

## Keys

Press `?` or `C-h` to open shortcut help as a popup over the current page.

**Tips**:

- Spotify category browsing (`g b`) is unavailable. Use Home, Library, or Search to find music.
- Use the `Search` command to search in shortcut help and other pages. In collection pages, `/` filters the visible rows; playback and actions target those rows. Press `Esc` to restore the full list while keeping the focused item.
- `RefreshPlayback` manually updates playback status.
- `SwitchProvider` (`g m`) changes browsing mode only; use
  `SwitchPlaybackProvider` (`g p`) to explicitly transfer playback ownership.
- `RestartIntegratedClient` is useful for switching audio devices without restarting the app.

List of supported commands:

| Command                         | Description                                                                                        | Default shortcuts  |
| ------------------------------- | -------------------------------------------------------------------------------------------------- | ------------------ |
| `NextTrack`                     | next track                                                                                         | `n`                |
| `PreviousTrack`                 | previous track                                                                                     | `p`                |
| `ResumePause`                   | resume/pause based on the current playback                                                         | `space`            |
| `PlayRandom`                    | play a random track in the current context                                                         | `.`                |
| `Repeat`                        | cycle the repeat mode                                                                              | `C-r`              |
| `Shuffle`                       | toggle the shuffle mode                                                                            | `C-s`              |
| `VolumeChange`                  | change playback volume by an offset (default shortcuts use 5%)                                     | `+`, `-`           |
| `Mute`                          | toggle playback volume between 0% and previous level                                               | `_`                |
| `SeekStart`                     | seek start of current track                                                                        | `^`                |
| `SeekForward`                   | seek forward by a duration in seconds (defaults to `seek_duration_secs`)                           | `>`                |
| `SeekBackward`                  | seek backward by a duration in seconds (defaults to `seek_duration_secs`)                          | `<`                |
| `Quit`                          | quit the application                                                                               | `C-c`, `q`         |
| `ClosePopup`                    | close a popup                                                                                      | `esc`              |
| `SelectNextOrScrollDown`        | select the next item in a list/table or scroll down (supports vim-style count: 5j)                 | `j`, `C-n`, `down` |
| `SelectPreviousOrScrollUp`      | select the previous item in a list/table or scroll up (supports vim-style count: 10k)              | `k`, `C-p`, `up`   |
| `ExtendSelectionNext`           | extend selection to the next item in a track list/table                                            | `J`, `S-down`      |
| `ExtendSelectionPrevious`       | extend selection to the previous item in a track list/table                                        | `K`, `S-up`        |
| `SelectAll`                     | select every visible item in the current keyed track pane                                          | `v a`              |
| `InvertSelection`               | invert the visible selection in the current keyed track pane                                       | `v i`              |
| `PageSelectNextOrScrollDown`    | select the next page item in a list/table or scroll a page down (supports vim-style count: 3C-f)   | `page_down`, `C-f` |
| `PageSelectPreviousOrScrollUp`  | select the previous page item in a list/table or scroll a page up (supports vim-style count: 2C-b) | `page_up`, `C-b`   |
| `SelectFirstOrScrollToTop`      | select the first item in a list/table or scroll to the top                                         | `g g`, `home`      |
| `SelectLastOrScrollToBottom`    | select the last item in a list/table or scroll to the bottom                                       | `G`, `end`         |
| `ChooseSelected`                | choose the selected item                                                                           | `enter`            |
| `RefreshPlayback`               | manually refresh the current playback                                                              | `r`                |
| `RestartIntegratedClient`       | restart the integrated client (`streaming` feature only)                                           | `R`                |
| `ShowActionsOnSelectedItem`     | open a popup showing actions on a selected item                                                    | `g a`, `C-space`   |
| `ShowActionsOnCurrentTrack`     | open a popup showing actions on the current track                                                  | `a`                |
| `ShowActionsOnCurrentContext`   | open a popup showing actions on the current context                                                | `A`                |
| `AddSelectedItemToQueue`        | add the selected item to queue                                                                     | `Z`, `C-z`         |
| `FocusNextWindow`               | focus the next focusable window (if any)                                                           | `tab`              |
| `FocusPreviousWindow`           | focus the previous focusable window (if any)                                                       | `backtab`          |
| `SwitchTheme`                   | open a popup for switching theme                                                                   | `T`                |
| `SwitchDevice`                  | open a popup for switching device                                                                  | `D`                |
| `SwitchProvider`                | switch between Spotify and YouTube Music                                                           | `g m`              |
| `SwitchPlaybackProvider`        | transfer playback ownership between Spotify and YouTube Music                                      | `g p`              |
| `Search`                        | open a popup for searching in the current page                                                     | `/`                |
| `BrowseUserPlaylists`           | open a popup for browsing user's playlists                                                         | `u p`              |
| `BrowseUserFollowedArtists`     | open a popup for browsing user's followed artists                                                  | `u a`              |
| `BrowseUserSavedAlbums`         | open a popup for browsing user's saved albums                                                      | `u A`              |
| `CurrentlyPlayingContextPage`   | go to the currently playing context page                                                           | `g space`          |
| `TopTrackPage`                  | go to the user top track page                                                                      | `g t`              |
| `RecentlyPlayedTrackPage`       | go to the user recently played track page                                                          | `g r`              |
| `LikedTrackPage`                | go to the user liked track page                                                                    | `g y`              |
| `LyricsPage`                    | go to the lyrics page of the current track                                                         | `g L`, `l`         |
| `RetryLyrics`                   | reload lyrics for the current lyrics page                                                         | `g R`              |
| `CycleLyricsSource`             | try the next enabled lyrics provider                                                             | `g C`              |
| `ToggleLyricsFollow`            | toggle automatic lyrics follow mode                                                               | `f`                |
| `LibraryPage`                   | go to the user library page                                                                        | `g l`              |
| `JournalPage`                   | go to the local track journal                                                                      | `g j`              |
| `JournalListsPage`              | go to saved journal lists                                                                          | `g J`              |
| `SessionHistoryPage`            | open local playback history                                                                        | `g h`              |
| `CreatePlaylistFromSessionHistory` | create a local Unified playlist from playback history                                           | `g H`              |
| `SettingsPage`                  | go to the settings page                                                                            | `g S`              |
| `SearchPage`                    | go to the search page                                                                              | `g s`              |
| `BrowsePage`                    | go to the browse page                                                                              | `g b`              |
| `Queue`                         | go to the queue page                                                                               | `z`                |
| `OpenCommandHelp`               | open shortcut help                                                                                  | `?`, `C-h`         |
| `PreviousPage`                  | go to the previous page                                                                            | `backspace`, `C-q` |
| `OpenLogs`                      | go the the application logs page                                                                   | `g o`              |
| `OpenSpotifyLinkFromClipboard`  | open a Spotify link from clipboard                                                                 | `O`                |
| `ImportYouTubeAuthFromClipboard` | write copied YouTube Music Cookie/OAuth credentials to the configured path (custom binding only)       | unbound            |
| `SortTrackByTitle`              | sort the track table (if any) by track's title                                                     | `s t`              |
| `SortTrackByArtists`            | sort the track table (if any) by track's artists                                                   | `s a`              |
| `SortTrackByAlbum`              | sort the track table (if any) by track's album                                                     | `s A`              |
| `SortTrackByAddedDate`          | sort the track table (if any) by track's added date                                                | `s D`              |
| `SortTrackByDuration`           | sort the track table (if any) by track's duration                                                  | `s d`              |
| `SortLibraryAlphabetically`     | sort the library alphabetically                                                                    | `s l a`            |
| `SortLibraryByRecent`           | sort the library (playlists and albums) by recently added items                                    | `s l r`            |
| `ReverseTrackOrder`             | reverse the order of the track table (if any)                                                      | `s r`              |
| `MovePlaylistItemUp`            | move playlist item up one position                                                                 | `C-k`              |
| `MovePlaylistItemDown`          | move playlist item down one position                                                               | `C-j`              |
| `CreatePlaylist`                | create a new playlist                                                                              | `N`                |
| `LinkUnifiedPlaylistToYouTube`  | link the current Unified playlist to an existing YouTube playlist                                | playlist actions   |
| `SyncUnifiedPlaylistToYouTube`  | append missing items from the current Unified playlist to its YouTube link                       | playlist actions   |
| `UnlinkUnifiedPlaylistFromYouTube` | remove the current Unified playlist's YouTube link                                             | playlist actions   |
| `RenameJournalList`             | rename the current journal list                                                                     | `e`                |
| `DeleteJournalList`             | delete the current journal list                                                                     | `delete`           |
| `JumpToCurrentTrackInContext`   | jump to the current track in the context                                                           | `g c`              |
| `JumpToHighlightTrackInContext` | jump to the currently highlighted search result in the context                                     | `C-g`              |

To add or modify shortcuts, see the [keymaps section](config.md#keymaps).

## Actions

Not all actions are available for every item or provider. To see available
actions, use `ShowActionsOnCurrentTrack` or `ShowActionsOnSelectedItem`, then
press enter to trigger the action. Some actions may not appear in a popup but
can be bound to shortcuts. Journal actions write to the local journal and can
retain YouTube identities; provider library and navigation actions remain
provider-scoped.

List of available actions:

- `GoToArtist`
- `GoToAlbum`
- `GoToRadio`
- `GoToShow`
- `AddToLibrary`
- `AddToPlaylist`
- `AddToQueue`
- `AddToLiked`
- `AddToJournalList`
- `AddAlbumToJournalList`
- `AddArtistTracksToJournalList`
- `AddToListenLater`
- `MarkListened`
- `MarkUnlistened`
- `DeleteFromLiked`
- `DeleteFromLibrary`
- `RemovePlaylistFromLibrary`
- `RenamePlaylist`
- `DeletePlaylist`
- `DeleteFromPlaylist`
- `EditNote`
- `ClearNote`
- `RemoveFromJournal`
- `RemoveFromJournalList`
- `RemoveFromListenLater`
- `SetRating`
- `ShowActionsOnAlbum`
- `ShowActionsOnArtist`
- `ShowActionsOnShow`
- `ShowJournalActions`
- `ToggleLiked`
- `CopyLink`
- `CopyLyrics`
- `CopyTimedLyrics`
- `RetryLyrics`
- `CycleLyricsSource`
- `BackupUnifiedPlaylistToListenBrainz`
- `CheckUnifiedPlaylistListenBrainzSync`
- `RefreshUnifiedPlaylistListenBrainzSync`
- `RetryUnifiedPlaylistListenBrainzSync`
- `PreviewUnifiedPlaylistListenBrainzPush`
- `PreviewUnifiedPlaylistListenBrainzPull`
- `ReviewUnifiedPlaylistListenBrainzConflicts`
- `RecoverUnifiedPlaylistListenBrainzSync`
- `RollbackUnifiedPlaylistListenBrainzPull`
- `OpenUnifiedPlaylistListenBrainzSync`
- `InitializeUnifiedPlaylistListenBrainzBase`
- `ApplyUnifiedPlaylistListenBrainzPush`
- `ApplyUnifiedPlaylistListenBrainzPull`
- `ApplyUnifiedPlaylistListenBrainzResolve`
- `LinkUnifiedPlaylistToYouTube`
- `SyncUnifiedPlaylistToYouTube`
- `UnlinkUnifiedPlaylistFromYouTube`
- `Follow`
- `Unfollow`
- `ClearSessionHistory`

Actions can also be bound to shortcuts. To add new shortcuts, see the [actions section](config.md#actions).

## Search page

When entering the search page, focus is on the search input. Enter text, use `backspace` to delete, and `enter` to search. Submitting a query moves focus to the selected result category (or the first result group), so global shortcuts work immediately. Press `esc` from results to edit the query again.

To move focus from the search input to other windows (track results, album results, etc.), use `FocusNextWindow` or `FocusPreviousWindow`.

The search status line distinguishes a request that is still running from an
empty result, a handled failure, or a newer request replacing an older one.
Failure notices use a short safe reference and a recommended next action;
they do not display provider error chains or request details.

## Command line

`unified-player` provides several CLI commands for interacting with Spotify:

- `get`: Get Spotify data (playlist/album/artist data, user's data, etc)
- `playback`: Interact with the playback (start a playback, play-pause, next, etc)
- `search`: Search spotify
- `connect`: Connect to a Spotify device
- `like`: Like currently playing track
- `authenticate`: Authenticate the application
- `youtube`: Sign in to YouTube Music, check metadata/playback auth, and run account actions
- `diagnostics`: Print a credential-safe build/configuration report; add `--live` to inspect provider and playback state from a running TUI
- `unified`: Manage, link, export, and project local provider-neutral playlists (`list`, `new`, `add`, `remove`, `delete`, `link`, `export --format jspf`, `project`)
- `listenbrainz`: Configure a token, inspect status, back up, restore, diff, or plan a remote playlist sync
- `playlist`: Playlist editing (new, delete, import, fork, etc)

For more details, run `unified-player -h` or `unified-player {command} -h`.

**Notes**

- On first launch, the setup screen signs you in; `unified-player authenticate` does the same from the command line.
- CLI commands communicate with a client socket on port `client_port` (default: `8080`). If no instance is running, a new client is started, which may increase latency.

### Scripting

The command-line interface is script-friendly. Use the `search` subcommand to retrieve Spotify data in JSON format, which can be processed with tools like [jq](https://jqlang.github.io/jq/).

Example: Start playback for the first track from a search query:

```sh
read -p "Search spotify: " query
unified-player playback start track --id $(unified-player search "$query" | jq '.tracks.[0].id' | xargs)
```
