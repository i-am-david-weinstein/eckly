# eckly

A keyboard-driven terminal client for [Readeck](https://readeck.org), built with
Rust and [Ratatui](https://ratatui.rs).

Read your saved articles, search your library, and manage bookmarks without
leaving the terminal.

![eckly bookmark list using the Ayu Dark theme](screenshots/list.png)

The reader turns Readeck's article HTML into readable, wrapped text with
headings, lists, quotes, code blocks and inline images:

![eckly reading view using the Ayu Dark theme](screenshots/reader.png)

The Highlights view groups every highlight by bookmark and follows the article
order; press `Enter` to jump to one:

![eckly highlights view using the Ayu Dark theme](screenshots/highlights.png)

## Features

- **Browse** the inbox (all non-archived bookmarks, including finished ones),
  reading progress, favorites and archives, or filter by label or collection.
- **Search** your library with Readeck's full-text search, or search within the
  current article with highlighted matches.
- **Annotate** paragraphs with colored highlights and notes. The Library pane
  lists every highlight by bookmark, and the reader browses or edits them.
- **Read** extracted articles in a comfortable, scrollable reader. Plain text is
  reflowed to the terminal width; headings, lists, quotes, code and images are
  rendered inline. Images use truecolor half blocks and are fetched from
  Readeck's media route.
- **Manage** bookmarks: mark read/unread, favorite, archive, edit labels,
  title, and short notes, or delete with a confirmation.
- **Bulk edit** a selection: press `Space` to select bookmarks, then apply
  `m`, `f`, `a`, `t` or `D` to the whole selection.
- **Save links** directly from the TUI.
- **Offline cache**: bookmark metadata and article text are cached locally, so
  cached bookmarks remain browsable and readable when Readeck is unreachable.
- **Progress tracking**: the reader resumes at the bookmark’s saved percentage
  and sends the updated position to Readeck when you leave.
- **Lazy pagination** and live sorting, without blocking the UI.
- **Interactive setup** on first launch; credentials are stored with `0600`
  permissions.
- **Themes** — all 39 built-in [opaline](https://crates.io/crates/opaline)
  themes, with a live-preview picker.
- **Label completion** — `Tab` cycles matching labels in the label editor.
- Responsive layout, spinner/progress feedback, error messages, and a complete
  help overlay (`?`).

## Installation

Requires Rust 1.85 or newer.

```sh
cargo install --git https://github.com/gollyhatch/eckly
```

Or from a local checkout:

```sh
cd eckly
cargo install --path .
```

Or build a binary with `cargo build --release` (the result is
`target/release/eckly`).

## Configuration

On first launch, `eckly` asks for your Readeck server URL and either an API token
or OAuth device login (`Ctrl+O`). To use a token, create one in Readeck under
**Settings → API tokens**. The configuration is saved to:

- `$XDG_CONFIG_HOME/eckly/config.toml`, or
- `~/.config/eckly/config.toml` when `XDG_CONFIG_HOME` is not set.

The file contains the server URL, token and optional theme, and is written with
mode `0600` on Unix. A configuration file that exists but cannot be read or
parsed aborts startup with an error instead of showing the setup screen.

```toml
server = "https://readeck.example.com"
token = "your-api-token"
theme = "catppuccin-mocha"
images = true
terminal_graphics = false
text_width = 100
text_align = "center"
speed_read_wpm = 300
```

`images` controls whether inline article images are rendered; it defaults to
`true`. Pressing `i` in the reader toggles images for the current session.

`terminal_graphics` (experimental, default `false`) draws images at full
resolution in terminals with a graphics protocol. At startup eckly asks the
terminal what it supports and uses the Kitty protocol (kitty, Ghostty), Sixel
(foot, xterm, Windows Terminal) or iTerm2 (iTerm2, WezTerm), and falls back to
half blocks otherwise. Images keep their natural size, shrunk to fit the
reading column. Inside tmux, the Kitty protocol needs
`set -g allow-passthrough on`.

`text_width` sets the maximum width of the reading column before text wraps.
`0` (the default) uses the full available width. When the terminal is wider than
`text_width`, `text_align = "center"` (the default) centers the column; set
`text_align = "left"` to align it with the left edge instead. Press `A` in the
reader to toggle alignment for the current session.

`speed_read_wpm` sets the speed reading pace in words per minute (default
`300`). Press `s` in the reader to show one word at a time, starting at the
current position. `Space` pauses or resumes, `h`/`l` or `←`/`→` step one word,
`+`/`-` change the pace for the session, and `Esc` returns to the article at
the word you reached.

Credentials can also be supplied per run:

```sh
# Command line arguments
eckly --server https://readeck.example.com --token "$READECK_TOKEN"

# Environment variables
READECK_URL=https://readeck.example.com READECK_TOKEN=... eckly

# Alternative token variable
READECK_API_TOKEN=... eckly

# A different config file
eckly --config ~/.config/eckly/work.toml

# Choose a theme without editing the config
eckly --theme gruvbox-dark
ECKLY_THEME=catppuccin-mocha eckly
```

Themes come from opaline’s 39 built-in themes, including Ayu, Catppuccin,
Dracula, Everforest, Gruvbox, Kanagawa, Nord, One, Rose Pine, Solarized,
Tokyo Night and the SilkCircuit family. **Ayu Dark** is the default theme. Run
`eckly --list-themes` to see every ID and display name. The theme is resolved from `--theme`, then `ECKLY_THEME`,
then the config file. Press `Ctrl+T` anywhere in the app to open a theme picker:
`j`/`k` preview the highlighted theme live, `Enter` applies it for the current
session, and `Esc` cancels. The old `default`, `gruvbox` and `catppuccin` names
are still accepted as aliases.

Precedence: command line arguments, then environment variables, then the
configuration file. Credentials supplied via arguments or environment are not
written back to the configuration file.

The token needs access to bookmarks and (for the username in the title bar) the
profile. If the token is restricted, eckly still works with whatever endpoints
are permitted and reports `403` errors in the status bar.

### OAuth

On the setup screen, enter the server URL and press `Ctrl+O` instead of providing
an API token. eckly registers an ephemeral OAuth client, requests a device code,
and displays a verification URL and user code. After you approve the request in
your browser, eckly polls Readeck and stores the resulting access token in the
config file like a regular API token.

The device flow requests `bookmarks:read`, `bookmarks:write` and `profile:read`
scopes. API tokens remain available for scripted setups, `--token`, and
environment-based configuration.

### Offline cache

eckly caches bookmark metadata, labels, collections, annotations, the user
profile and article HTML under:

- `$XDG_CACHE_HOME/eckly`, or
- `~/.cache/eckly` when `XDG_CACHE_HOME` is not set.

When Readeck cannot be reached, the help line shows **OFFLINE** and eckly falls
back to the cached bookmark list. Filters, metadata search and sorting work
locally; collection views require a connection. Opening a cached bookmark loads
its saved article text.

Changes made offline — read state, favorites, archive state, labels, title, note
and deletion — are applied locally and stored in a pending queue. The status
line shows the number of pending changes. The queue is sent automatically when
Readeck becomes reachable again, including on the next online launch. Creating
new bookmarks still requires a connection.

Article images and full-text search are not cached; offline search matches
titles, descriptions, notes, URLs and labels. The cache is not automatically
evicted.

Start the TUI in forced offline mode with:

```sh
eckly --offline
```

This never contacts Readeck: it browses the local cache and opens cached
articles. Bookmark changes are queued locally and synced on the next online
launch. `r` reloads the cache from disk, which is useful after running
`eckly sync` in another terminal.

### Sync for offline reading

The `sync` subcommand downloads bookmark metadata, article HTML and
annotations without starting the TUI. Run without flags to sync incrementally:
it replays Readeck's change log since the previous sync, applies updates, drops
bookmarks deleted on the server, and downloads the article of every new or
changed bookmark.
Articles already cached for unchanged bookmarks are left alone. The first run
has no watermark, so it snapshots the whole library and downloads every article
that is not cached yet.

```sh
eckly sync                # everything that changed since the last sync
eckly sync --unread       # the Unread list (all non-archived bookmarks)
eckly sync --favorites    # favorite bookmarks
eckly sync --archived     # archived bookmarks
eckly sync --all          # everything
```

The watermark is stored in the cache and advanced only after every change is
applied, so a failed run is retried by the next one.

Flags can be combined to sync the union of several lists:

```sh
eckly sync --unread --favorites
```

Pending offline changes are uploaded first. The command uses the same config
file, `--server`/`--token`, or environment variables as the TUI. In an
interactive terminal it displays progress bars via `indicatif`; when
stdout/stderr are not terminals (for example from cron), it prints nothing on
success and only reports errors.

## Keys

`?` opens a full help overlay at any time. `Ctrl+C` quits immediately.

### Bookmark list

The **Unread** view is an inbox: it shows all bookmarks that are not archived,
including ones you have finished.

`Space` toggles a bookmark for bulk actions. When a selection exists, `m`, `f`,
`a`, `t` and `D` apply to every selected bookmark; otherwise they act on the
highlighted bookmark. Changing the filter, search or sort clears the selection.
Opening the reader keeps it. Successful `m`/`f`/`a`/`t` actions keep the
selection unless the updated bookmarks leave the current filter. Deleting removes
the deleted bookmarks from the selection. Title and note editing still act on
the highlighted bookmark only. Deleting a selection always shows a confirmation
first.

| Key | Action |
| --- | --- |
| `j` / `↓`, `k` / `↑` | Move selection |
| `Space` | Select / deselect the highlighted bookmark |
| `Ctrl+a` | Select all bookmarks currently loaded in the list |
| `PgDn` / `PgUp` | Move one page |
| `Ctrl+d` / `Ctrl+u` | Move half a page |
| `g` / `G` | First / last bookmark |
| `Enter` / `l` / `→` | Open the reader |
| `Tab` / `h` / `←` | Focus the filter sidebar |
| `Esc` | Clear search, then selection; otherwise focus the Library panel |
| `m` | Toggle read / unread |
| `f` | Toggle favorite |
| `a` | Archive / restore |
| `t` | Edit labels (comma separated) |
| `T` | Edit title |
| `E` | Edit note |
| `D` | Delete highlighted/selected bookmarks (with confirmation) |
| `o` | Open the original page in your browser |
| `/` | Search (empty query clears; `Ctrl+w` deletes the previous word) |
| `s` / `S` | Cycle sort order forward / backward |
| `n` | Save a new link |
| `r` | Refresh bookmarks, labels, collections and profile |
| `q` | Quit |

### Search filters

The `/` search box sends free text to Readeck's full-text search and also
understands `key:value` filters anywhere in the query:

| Filter | Values | Effect |
| --- | --- | --- |
| `type:` | `article`, `photo`, `video` | Bookmark type (repeatable) |
| `lang:` | language code, `*` wildcard | Bookmark language |
| `label:` | label name (quote names with spaces) | Bookmark labels (repeatable) |
| `status:` | `unread`, `reading`, `read` | Reading progress (repeatable) |
| `is:` | `favorite`, `archived`, `loaded` | Flags |
| `has:` | `labels`, `notes`, `errors` | Content present |
| `after:`, `before:` | time token (`-7d`, `now`, `2026-01-01`) | Creation date range |
| `id:` | bookmark ID | Match specific bookmarks |

A filter replaces the matching option of the current view: `is:archived`
searches the archive from any view, and `status:read` replaces the progress
filter. Everything else is passed to Readeck unchanged, so its field syntax
(`title:`, `author:`, `site:`, `note:`), quoted phrases, `*` wildcards
and `-` negation keep working. The filters also apply to the offline cache;
field terms are server-side only, so offline they are matched as plain text.

### Filter sidebar

| Key | Action |
| --- | --- |
| `j` / `k` | Move between filters |
| `PgUp` / `PgDn` | Move one page |
| `Ctrl+u` / `Ctrl+d` | Move half a page |
| `g` / `G` | First / last filter |
| `/` | Filter labels and collections |
| `R` | Rename the selected label |
| `Enter` / `l` / `→` | Apply the selected filter |
| `Tab` | Return to the bookmark list |
| `Esc` / `h` / `←` | Clear the library filter, then return |

While the library filter is active, type to narrow the list, use `↑`/`↓` to
choose a result, and press `Enter` to apply it. `Esc` clears the filter.

Selecting a label shows every bookmark carrying it, regardless of reading
progress or archive status.

### Reader

| Key | Action |
| --- | --- |
| `j` / `↓`, `k` / `↑` | Scroll one line |
| `Space` | Scroll to the next paragraph |
| `PgDn` | Page down |
| `PgUp` / `b` | Page up |
| `Ctrl+d`, `Ctrl+u` | Half page down / up |
| `g` / `G` | Top / bottom |
| `/` | Search the article text |
| `n` / `N` | Next / previous search match |
| `m`, `f`, `a` | Toggle read, favorite, archive |
| `t`, `T`, `E` | Edit labels, title, note |
| `D` | Delete the bookmark (with confirmation) |
| `i` | Enable / disable inline images |
| `A` | Toggle centered / left-aligned text |
| `s` | Speed reading (RSVP) |
| `v` | Start a highlight selection |
| `H` | Annotations (view, add, edit, delete) |
| `o` | Open in browser |
| `r` | Retry loading the article |
| `Esc` | Clear the article search, then return to the list |
| `q` | Return to the list |

While speed reading, `Space` pauses or resumes, `h`/`l` (or `←`/`→`) step one
word and pause, `+` / `-` change the pace by 25 words per minute, and `s` or
`Esc` returns to the article.

### Annotations

Highlights from Readeck are shown in the reader: the annotated text is drawn in
its color, and a highlight with a note shows it inline as `[note]`. In the
bookmark list, each highlight is listed under its bookmark and the metadata line
shows how many highlights the bookmark has. The Library pane has a
**Highlights** view below **All Bookmarks** that lists every highlight in the
library, grouped by bookmark and in article order; press `Enter` on one to read
its bookmark at that highlight, or `D` to delete the highlight. `v` starts a
text selection directly; `H` opens the annotation list, where `n` starts one
too, `e` edits the note, `c` cycles the color (`yellow`, `red`, `green`, `blue`,
`purple`), `d` deletes it and `Enter` jumps to it. Annotations made in
Readeck's web UI are shown on the paragraph that contains them.

In selection mode, move the cursor with `h`/`j`/`k`/`l`, jump by word with
`w`/`b`/`e` (or `W`/`B`/`E` for whitespace-delimited words), jump to the start
or end of the line with `0`/`^`/`$`, and jump between paragraphs with `{`/`}`.
`Enter` or `Space` sets the start of the selection and a second press finishes
it. The selected text is highlighted as you move, and finishing creates a
yellow highlight that you can give a note or another color from the list.
Inline notes are hidden while selecting so they cannot end up in a selection.
`Esc` cancels the selection.
`eckly sync` downloads annotations too, so highlights stay visible offline;
adding or editing them needs a connection to Readeck.

### General

| Key | Action |
| --- | --- |
| `?` | Toggle help overlay |
| `Ctrl+T` | Choose color theme |
| `Ctrl+C` | Quit eckly |

### Text fields

Text entry fields (search, labels, title, note, new bookmark and setup) support:

| Key | Action |
| --- | --- |
| `Ctrl+a` | Move cursor to the beginning |
| `Ctrl+e` | Move cursor to the end |
| `Ctrl+w` | Delete the previous word |
| `Alt+←` / `Alt+→` | Jump to the previous / next label in the label editor |
| `Tab` | Complete / cycle labels in the label editor |

## Notes and limitations

- Images are rendered as truecolor half blocks unless `terminal_graphics` is
  on. Terminals without truecolor will approximate the colors, and animated
  images show their first frame. Up to 12 images are loaded per article.
  Embedded videos and complex tables are flattened; `o` opens the original
  page when needed.
- Notes are edited on a single line. Notes containing multiple lines should be
  edited in the Readeck web UI to avoid flattening them.
- Label names containing commas cannot be entered from the TUI.
- Deleting a bookmark is immediate after confirmation. Readeck's archive is a
  safer option if you may want the article later.
- The client targets the Readeck HTTP API (`/api/bookmarks`,
  `/api/bookmarks/{id}/article`, labels, collections and profile). It has no
  offline cache.

## Development

```sh
cargo test       # unit tests, HTML extraction, API request/response tests
cargo clippy --all-targets
cargo fmt
```

The code is split into a small API client (`src/api.rs`), domain models
(`src/models.rs`), the application state machine (`src/app.rs`), HTML-to-text
conversion (`src/article.rs`), and the Ratatui views (`src/ui/`). The UI never
performs blocking network calls; API work runs in Tokio tasks and reports back
through an event channel.

## License

GNU General Public License v3.0 or later. See [LICENSE](LICENSE).
