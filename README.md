# proLogue
## TUI-ELN with version-controlled private storage

A small terminal-based electronic lab notebook/bullet journal written in Rust.
It stores timestamped journal entries and tasks in a JSON file that lives in a **separate** Git repository (so your data can be private while the program repo stays public). The UI is text-based (ratatui + crossterm) and divided into four quadrants you navigate with `Tab` / `Shift+Tab`.

## Features

* Terminal UI split into 4 quadrants:

  * **ENTRY** (top-right) — type or paste text. `Enter` submits, `Shift+Enter` inserts newline.
  * **TASKS** (top-left) — tasks created by starting an entry with `*`. Select and press `Enter` to mark done (idempotent: only first `Enter` marks it done and adds a `DONE:` journal entry).
  * **SEARCH** (bottom-left) — search by words, comma-separated terms, or date `YYYY_MM_DD`. Press `Enter` with input to run a search; clear input and press `Enter` to open a selected result.
  * **JOURNAL** (bottom-right) — shows today’s entries (reverse chronological).
* Keyboard-first navigation:

  * `Tab` → advance focus (counter-clockwise): `ENTRY -> TASKS -> SEARCH -> JOURNAL -> ENTRY ...`
  * `Shift+Tab` → previous focus
  * `Up` / `Down` arrows → scroll within focused quadrant (where applicable)
  * `Enter` / `Shift+Enter` rules documented above
  * `Ctrl+C` → quit
  * `Ctrl+G` in SEARCH → run search (alternate)

* One automatic **Clock-in / Clock-out** entry on startup (created once per day):
  * `CLOCK_IN_TIME = now`
  * `CLOCK_OUT_TIME = CLOCK_IN_TIME plus your work hours (currently + 9 hours` on Monday/Tuesday, else `+8 hours` for a 41 hr work week)
  * Entry text: `Clock-in: {CLOCK_IN_TIME}, Clock-out: {CLOCK_OUT_TIME}`
  * This entry is created only once per day (if present, program won’t duplicate it).
* Each entry & task saved to JSON file, which is committed (and pushed) to a separate Git repository. New entries generate an automated commit.
* Entries and tasks have `tags` (used for searching); session `LOCATION` (entered at startup) is stored as a tag — not appended to visible text.
* Backwards-compatible JSON deserialization: older JSON files without `tags` load correctly.

* The program attempts a best-effort commit & push using the credentials posted at the top of the script. If you do not want the program to push, toggle `COMMIT_AND_PUSH` in the source.
* The program will try to interpret configured repo paths to be absolute (it expands `~` and converts `./Users/...` / `Users/...` to `/Users/...`). See *Configuration* below.

## Installation & build

Prerequisites:

* Rust toolchain (rustup/cargo). Tested with Rust 1.XX+ (use `rustup`).
* (Optional) `git` and network/SSH credentials if you enable push.

Steps:

```bash
# clone the program repository
git clone https://github.com/chasenunez/pro_logue.git
cd pro_logue

# build
cargo build --release

# run
cargo run --release
```

On startup, the app will ask for `LOCATION:` in the terminal — type a short label and press Enter. The program will use that value as a tag added to new entries and tasks created during the session. This allows you to later search for all the entries you have made at a specific location. 

## Configuration

Open `src/main.rs`. At the top of the file you will find configuration constants; the most important ones:

```rust
const GIT_REPO_PATH: &str = "/Users/you/path/to/private_journal_repo/";
const DATA_FILE_NAME: &str = "journal.json";
const COMMIT_AND_PUSH: bool = true; // set false to disable committing/pushing
const AUTHOR_NAME: &str = "Your Name";
const AUTHOR_EMAIL: &str = "you@example.com";
const GIT_REMOTE_NAME: &str = "origin";
const GIT_BRANCH: &str = "main";
```

**Important**: `GIT_REPO_PATH` must resolve to an absolute path outside your program repo. The program performs path normalization (expands `~` and converts `./Users/...` or `Users/...` into `/Users/...`) so the directory you configure is used directly at the filesystem root.

If the target directory does not exist the program will attempt to create it.

To disable pushing to remote, set `COMMIT_AND_PUSH` to `false`; the program will still write and commit locally (if you want to disable commits as well, you can further modify the `git_commit_and_push` function).

## Data format

`journal.json` is a simple JSON file with two top-level keys:

```json
{
  "journal": {
    "2025_12_18": [
      { "time": "09:00", "text": "Clock-in: 09:00, Clock-out: 17:00", "tags": ["EAWAG"] },
      { "time": "10:45", "text": "Met with team...", "tags": [] }
    ]
  },
  "tasks": [
    { "id": 1, "text": "Fix bug", "created_at": "2025-12-18T10:00:00", "completed_at": null, "tags": ["EAWAG"] }
  ]
}
```

Notes:

* `tags` is a list of strings and is used for searching. The program is backwards-compatible with older files that did not include `tags` thanks to serde defaults.
* Clock entries and DONE journal lines are plain text entries by default; if you want more structured clock entries we can change the schema later.

## Contributing

Contributions welcome. If you want to:

* Add unit tests,
* Convert the app into a multi-file crate,
* Add an explicit configuration file instead of compile-time constants,
* Make the clock entry structured (JSON object),
* Or add a help overlay and theming,

open a PR or an issue describing the change. Please follow Rust's idioms and keep changes minimal and well-documented.


## License

MIT License Copyright (c) 2025 Chase Núñez
