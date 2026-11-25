//! Sterling Quest Sync Companion desktop app: the window and the commands it can call.
//!
//! The web page has no file or shell access. It can only call the typed commands below, and it
//! never hands the app a path: folders are picked in a native dialog on the Rust side and checked.

// Sterling code style puts a blank line between "}" and "else" on purpose.
#![allow(clippy::suspicious_else_formatting)]
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;
use std::sync::mpsc::{channel, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use tauri::menu::{Menu, MenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Emitter, Manager, State};
use tauri_plugin_dialog::DialogExt;
use tauri_plugin_updater::UpdaterExt;

use sqs_core::api::{self, Api, Client};
use sqs_core::engine::{self, Report};
use sqs_core::formats::{parse_outbox, AppInfo, CharacterOut, Kind, Look, Outbox, PartnerApp};
use sqs_core::game::{GameCheck, SaveWatcher};
use sqs_core::keystore::{KeyStore, SystemStore};
use sqs_core::paths::{self, GameFolder};
use sqs_core::safety::{write_app_file, SafeFs};
use sqs_core::{companion, notices, updater, Error};

// The version people see (window, tray, logs, server): "0.1.28.01" for an internal build. The package
// version ("0.1.29-dev.1") is only for the updater, so it never offers an internal build an older release.
const VERSION: &str = env!("SQS_DISPLAY_VERSION");
const PACKAGE_VERSION: &str = env!("CARGO_PKG_VERSION");
const MAX_ACTIVITY: usize = 200;
const STERLING_CHECK_EVERY: Duration = Duration::from_secs(6 * 3600);
/// Syncs also look for a new app version, at most this often, so a fix shows on Home within the hour.
const APP_CHECK_AT_SYNC: Duration = Duration::from_secs(3600);
const MAX_OUTBOX_BYTES: u64 = 16 * 1024 * 1024;

// Companion add-on files, bundled at build time. Inbox.lua is never bundled.
const COMPANION_FILES: &[(&str, &[u8])] = &
[
    ("SterlingQuestSync.toc", include_bytes!("../companion/SterlingQuestSync.toc")),
    ("Core.lua", include_bytes!("../companion/Core.lua")),
    ("icon.tga", include_bytes!("../companion/icon.tga")),
];

/// Unix time in seconds.
fn now() -> i64
{
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

/// The app's own folder for settings, state and backups.
fn app_data() -> PathBuf
{
    dirs::data_dir().unwrap_or_else(std::env::temp_dir).join("SterlingQuestSync")
}

// ------------------------------------------------------------------ settings

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
struct Settings
{
    game: Option<PathBuf>,
    server: String,
    pc_label: String,
    joined: bool,
    auto_sync: bool,
    auto_update_sterling: bool,
    /// Off by default: the app asks before updating itself.
    auto_update_app: bool,
    /// The X button hides the window to the tray (keeps syncing) instead of quitting.
    close_to_tray: bool,
    character: Option<String>,
    /// A new add-on (the companion or Sterling) was added while the game may have been open.
    /// WoW only finds new add-on folders at startup, so the game needs a full restart.
    /// Cleared once the game is seen closed.
    restart_game: bool,
    /// Where the window was last, so it opens there again.
    window: Option<Placement>,
    /// Off by default and never suggested: the person turns it on in Settings.
    start_with_windows: bool,
    /// Set when Start with Windows was turned off outside the app (Task Manager, Windows Settings or
    /// another program). The app never turns it back on by itself; Settings explains why it's off.
    autostart_note: Option<String>,
    /// Off by default: a Windows notification when a favourite's new progress arrives.
    notify_favs: bool,
    /// The one-time "still running in the tray" note was shown.
    close_note_seen: bool,
    /// The version whose "What's new" was last seen. None on a fresh install.
    seen_version: Option<String>,
    /// The Companion installed the add-on itself. Only then does it keep the add-on updated
    /// when an add-on manager (WowUp, the CurseForge app) is on this PC, so the two don't fight.
    sterling_by_app: bool,
    /// Notices from the server that the person closed. Kept short.
    dismissed_notices: Vec<String>,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
struct Placement
{
    x: i32,
    y: i32,
    w: u32,
    h: u32,
    max: bool,
}

impl Default for Settings
{
    fn default() -> Self
    {
        Settings
        {
            game: None,
            server: api::DEFAULT_SERVER.into(),
            pc_label: default_label(),
            joined: false,
            auto_sync: true,
            auto_update_sterling: true,
            auto_update_app: false,
            close_to_tray: true,
            character: None,
            restart_game: false,
            window: None,
            start_with_windows: false,
            autostart_note: None,
            notify_favs: false,
            close_note_seen: false,
            seen_version: None,
            sterling_by_app: false,
            dismissed_notices: Vec::new(),
        }
    }
}

/// This PC's name, used as its label when the person doesn't give one.
fn default_label() -> String
{
    let host = std::env::var("COMPUTERNAME").or_else(|_| std::env::var("HOSTNAME")).unwrap_or_else(|_| "My PC".into());

    clean_label(&host).unwrap_or_else(|| "My PC".into())
}

/// Labels the server accepts: letters, digits, spaces and a few marks, up to 40.
fn clean_label(raw: &str) -> Option<String>
{
    let cleaned: String = raw.trim().chars().filter(|c| c.is_alphanumeric() || " _.'()-".contains(*c)).take(40).collect();
    let cleaned = cleaned.trim().to_string();

    cleaned.chars().next().filter(|c| c.is_alphanumeric()).map(|_| cleaned)
}

/// Reads one of the app's JSON files, or the default if it's missing or broken.
fn load_json<T: for<'de> Deserialize<'de> + Default>(name: &str) -> T
{
    fs::read_to_string(app_data().join(name)).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default()
}

/// Writes one of the app's JSON files.
fn save_json<T: Serialize>(name: &str, value: &T) -> Result<(), String>
{
    let text = serde_json::to_string_pretty(value).map_err(|e| e.to_string())?;

    write_app_file(&app_data(), name, text.as_bytes()).map_err(msg)
}

// ------------------------------------------------------------------ state

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Activity
{
    t: i64,
    text: String,
    /// "good", "bad" or "" for neutral.
    tone: String,
}

struct Session
{
    fs: SafeFs,
    state: engine::State,
}

struct Inner
{
    settings: Settings,
    session: Option<Session>,
    game_running: bool,
    last_error: Option<String>,
    online: Option<bool>,
    rejoin: bool,
    activity: Vec<Activity>,
    sterling_latest: Option<String>,
    app_latest: Option<String>,
    /// A signed app update the updater found, ready to install.
    app_update: Option<String>,
    sterling_checked: Option<Instant>,
    busy: bool,
    /// What an app update in progress is doing, for the Home card.
    updating: Option<UpdateStep>,
    /// Folder shown in setup step 1, not used until Continue.
    candidate: Option<PathBuf>,
    /// When the server first stopped answering. Short blips aren't shown.
    offline_since: Option<i64>,
    /// Add-on managers seen on this PC (WowUp, the CurseForge app).
    managers: Vec<String>,
    /// The owner's notices from the server, as last fetched (filtered when shown).
    notices: Vec<notices::Notice>,
    /// Why the last app update failed, until the next try. Home then offers a manual download.
    update_failed: Option<String>,
    /// What the last sync skipped and why, with character ids swapped for names. Logged again only when it changes.
    last_skipped: Vec<String>,
    /// When a sync last looked for a new app version.
    app_checked_at_sync: Option<Instant>,
}

#[derive(Serialize, Clone)]
struct UpdateStep
{
    step: String,
    /// 0 to 100 while downloading, absent when there's no number to show.
    percent: Option<u8>,
}

struct AppState
{
    inner: Mutex<Inner>,
    keys: Arc<dyn KeyStore>,
    /// Restarts the save watcher after the game folder changes.
    rewatch: Mutex<Option<std::sync::mpsc::Sender<()>>>,
}

/// A core error as the text the window shows.
fn msg(error: Error) -> String
{
    error.to_string()
}

/// Opens the game folder with its safe file access and saved sync state.
fn open_session(game: &PathBuf) -> Result<Session, String>
{
    let fs_ = SafeFs::new(game, &app_data()).map_err(msg)?;
    let state = engine::load_state(&fs_);

    Ok(Session { fs: fs_, state })
}

impl AppState
{
    fn client(&self, settings: &Settings) -> Result<Client, String>
    {
        let key = self.keys.get(&settings.server).map_err(msg)?.ok_or("This PC hasn't joined yet.")?;

        Client::new(&settings.server, &key).map_err(msg)
    }
}

impl Inner
{
    fn log(&mut self, text: impl Into<String>, tone: &str)
    {
        self.activity.insert(0, Activity { t: now(), text: text.into(), tone: tone.into() });
        self.activity.truncate(MAX_ACTIVITY);

        let _ = save_json("activity.json", &self.activity);
    }

    /// Display names for GUIDs: your own characters and your partners.
    fn names(&self) -> BTreeMap<String, String>
    {
        let mut map = BTreeMap::new();

        if let Some(session) = &self.session
        {
            for (guid, character) in &session.state.listed
            {
                map.insert(guid.clone(), character.name.clone());
            }

            for partner in &session.state.pairs
            {
                if !partner.partner_name.is_empty()
                {
                    map.insert(partner.partner_guid.clone(), partner.partner_name.clone());
                }
            }
        }

        map
    }
}

/// Reads every account's outbox, plus a note for each one that couldn't be read.
fn read_outboxes(fs_: &SafeFs) -> (Vec<Outbox>, Vec<String>)
{
    let mut out = Vec::new();
    let mut problems = Vec::new();

    for acct in paths::wtf_accounts(fs_.game())
    {
        let file = paths::outbox_file(fs_.game(), &acct);

        // A real outbox is well under a megabyte per character; never load something huge.
        if fs::metadata(&file).is_ok_and(|m| m.len() > MAX_OUTBOX_BYTES)
        {
            problems.push("outbox file too large".into());
            continue;
        }

        if let Ok(text) = fs::read_to_string(&file)
        {
            match parse_outbox(&text)
            {
                Ok(o) => out.push(o),
                Err(e) => problems.push(e.to_string()),
            }
        }
    }

    (out, problems)
}

// ------------------------------------------------------------------ status

#[derive(Serialize, Clone)]
struct PartnerStatus
{
    pairing_id: String,
    guid: String,
    name: String,
    realm: String,
    active: bool,
    /// "removed", "replaced", "deleted" or similar when the pairing ended.
    ended_reason: Option<String>,
    ended_at: Option<i64>,
    /// Game time of the newest copy from them on this PC.
    last_copy: Option<i64>,
    /// Sterling has applied that copy in game.
    applied: bool,
    /// Level, class and race from Sterling, for the window only.
    look: Look,
}

#[derive(Serialize, Clone)]
struct CharStatus
{
    guid: String,
    name: String,
    realm: String,
    /// Game time of the last save the app has seen, if any.
    saved: Option<i64>,
    ready: bool,
    look: Look,
    /// Favourite partners, in order (up to 5).
    favs: Vec<String>,
    /// Sterling's version as this character last saved it.
    sterling: Option<String>,
    partners: Vec<PartnerStatus>,
    /// Sterling partners with no live pairing in the app yet (names only if Sterling wrote them).
    unpaired: Vec<Unpaired>,
}

#[derive(Serialize, Clone)]
struct Unpaired
{
    guid: String,
    name: Option<String>,
    /// A code is in the inbox for Sterling to hand over in game.
    offered: bool,
    /// "yes", "no" or "unknown": whether their Sterling said in game that they have the app.
    app: &'static str,
    look: Look,
}

#[derive(Serialize, Clone)]
struct Status
{
    version: &'static str,
    setup_needed: bool,
    joined: bool,
    rejoin: bool,
    game: Option<GameFolder>,
    game_path: Option<String>,
    online: Option<bool>,
    pc_label: String,
    game_running: bool,
    auto_sync: bool,
    auto_update_sterling: bool,
    companion_installed: bool,
    sterling_version: Option<String>,
    sterling_latest: Option<String>,
    app_latest: Option<String>,
    app_update: Option<String>,
    auto_update_app: bool,
    close_to_tray: bool,
    restart_game: bool,
    start_with_windows: bool,
    autostart_note: Option<String>,
    notify_favs: bool,
    close_note_seen: bool,
    /// WoW saved Sterling's data without the helper add-on: it isn't loaded.
    helper_off: bool,
    /// Add-on managers on this PC, when one of them looks after the add-on instead of the Companion.
    sterling_managed_by: Vec<String>,
    /// Notes for versions newer than the one last seen, shown once after an update.
    whats_new: Vec<(String, Vec<String>)>,
    /// The owner's notices for this version, not dismissed.
    notices: Vec<notices::Notice>,
    update_failed: Option<String>,
    last_sync: Option<i64>,
    last_error: Option<String>,
    busy: bool,
    updating: Option<UpdateStep>,
    character: Option<String>,
    characters: Vec<CharStatus>,
    activity: Vec<Activity>,
}

/// Everything the window shows, gathered in one snapshot.
fn build_status(state: &AppState) -> Status
{
    let inner = state.inner.lock().unwrap();
    let settings = &inner.settings;
    let mut characters: Vec<CharStatus> = Vec::new();
    let mut companion_installed = false;
    let mut last_sync = None;
    let mut sterling_version = None;
    let mut game = None;
    let mut game_path = None;

    if let Some(sess) = &inner.session
    {
        companion_installed = companion::is_installed(&sess.fs);
        last_sync = sess.state.last_sync;
        sterling_version = updater::installed_version(sess.fs.game());
        game = paths::inspect(sess.fs.game());
        game_path = Some(sess.fs.game().display().to_string());

        let (outboxes, _) = read_outboxes(&sess.fs);
        let ready: Vec<String> = engine::pending(&sess.state, &outboxes).into_iter().map(|(g, _)| g).collect();
        let saved: BTreeMap<&str, (i64, &BTreeMap<String, i64>)> =
            outboxes.iter().flat_map(|o| &o.characters).map(|c| (c.guid.as_str(), (c.saved, &c.applied))).collect();
        // Characters seen in the outbox, plus any listed on the server from another PC.
        let mut guids: Vec<(String, String, String)> =
            outboxes.iter().flat_map(|o| &o.characters).map(|listed| (listed.guid.clone(), listed.name.clone(), listed.realm.clone())).collect();

        for (char_guid, listed) in &sess.state.listed
        {
            if !guids.iter().any(|x| &x.0 == char_guid)
            {
                guids.push((char_guid.clone(), listed.name.clone(), listed.realm.clone()));
            }
        }

        for (guid, name, realm) in guids
        {
            let inbox = sess.state.inbox.chars.get(&guid);
            let applied = saved.get(guid.as_str()).map(|x| x.1);
            let me: Option<&CharacterOut> = outboxes.iter().flat_map(|o| &o.characters).find(|c| c.guid == guid);
            let look_of = |g: &str| me.and_then(|c| c.partners.iter().find(|p| p.guid == g)).map(|p| p.look.clone()).unwrap_or_default();
            let partners = sess
                .state
                .pairs
                .iter()
                .filter(|p| p.my_guid == guid)
                .map(|partner|
                {
                    let last = inbox.and_then(|ch| ch.packets.iter().find(|x| x.from == partner.partner_guid && x.kind == Kind::Partner));

                    PartnerStatus
                    {
                        pairing_id: partner.id.clone(),
                        guid: partner.partner_guid.clone(),
                        name: partner.partner_name.clone(),
                        realm: partner.partner_realm.clone(),
                        active: partner.active,
                        ended_reason: partner.ended_reason.clone(),
                        ended_at: partner.ended_at,
                        last_copy: last.map(|x| x.time),
                        applied: last.is_some_and(|x| applied.and_then(|a| a.get(&partner.partner_guid)).is_some_and(|n| *n >= x.seq)),
                        look: look_of(&partner.partner_guid),
                    }
                })
                .collect();
            let live: Vec<&str> = sess.state.pairs.iter().filter(|p| p.my_guid == guid && p.active).map(|p| p.partner_guid.as_str()).collect();
            let unpaired: Vec<Unpaired> = outboxes
                .iter()
                .flat_map(|o| &o.characters)
                .filter(|c| c.guid == guid)
                .flat_map(|c| &c.partners)
                .filter(|p| !p.removed && !live.contains(&p.guid.as_str()))
                .map(|partner| Unpaired
                {
                    guid: partner.guid.clone(),
                    name: partner.name.clone(),
                    offered: sess.state.offers.get(&guid).is_some_and(|m| m.contains_key(&partner.guid)),
                    app: match partner.app
                    {
                        PartnerApp::Has(_) => "yes",
                        PartnerApp::No => "no",
                        PartnerApp::Unknown => "unknown",
                    },

                    look: partner.look.clone(),
                })
                .collect();

            characters.push(CharStatus
            {
                favs: sess.state.favs.get(&guid).map(|f| f.list.clone()).unwrap_or_default(),
                look: me.map(|c| c.look.clone()).unwrap_or_default(),
                sterling: me.and_then(|c| c.sterling.clone()),
                unpaired,
                ready: ready.contains(&guid),
                saved: saved.get(guid.as_str()).map(|x| x.0),
                guid,
                name,
                realm,
                partners,
            });
        }

        characters.sort_by(|a, b| b.saved.cmp(&a.saved).then(a.name.cmp(&b.name)));
    }

    Status
    {
        version: VERSION,
        setup_needed: inner.session.is_none() || !settings.joined,
        joined: settings.joined,
        rejoin: inner.rejoin,
        game,
        game_path,
        online: inner.online,
        pc_label: settings.pc_label.clone(),
        game_running: inner.game_running,
        auto_sync: settings.auto_sync,
        auto_update_sterling: settings.auto_update_sterling,
        companion_installed,
        sterling_version,
        sterling_latest: inner.sterling_latest.clone(),
        app_latest: inner.app_latest.clone(),
        app_update: inner.app_update.clone(),
        auto_update_app: settings.auto_update_app,
        close_to_tray: settings.close_to_tray,
        restart_game: settings.restart_game,
        start_with_windows: settings.start_with_windows,
        autostart_note: settings.autostart_note.clone(),
        notify_favs: settings.notify_favs,
        close_note_seen: settings.close_note_seen,
        helper_off: settings.game.as_deref().map(companion::helper_not_loaded).unwrap_or(false),
        whats_new: whats_new(settings.seen_version.as_deref()),
        notices: notices::for_app(&inner.notices, VERSION, &settings.dismissed_notices, now()),
        update_failed: inner.update_failed.clone(),
        sterling_managed_by: if settings.sterling_by_app { Vec::new() } else { inner.managers.clone() },
        last_sync,
        last_error: inner.last_error.clone(),
        busy: inner.busy,
        updating: inner.updating.clone(),
        character: settings.character.clone(),
        characters,
        activity: inner.activity.iter().take(60).cloned().collect(),
    }
}

/// Sends the window a fresh status.
fn emit_status(app: &AppHandle)
{
    let state = app.state::<AppState>();
    let _ = app.emit("status", build_status(&state));
}

// ------------------------------------------------------------------ commands: setup

#[tauri::command]
fn status(state: State<'_, AppState>) -> Status
{
    build_status(&state)
}

#[tauri::command(async)]
fn discover_games() -> Vec<GameFolder>
{
    paths::discover()
}

/// Switches to a new game folder and starts watching its saves.
fn set_game(app: &AppHandle, state: &AppState, path: PathBuf) -> Result<GameFolder, String>
{
    let picked_game = paths::normalise_pick(&path).ok_or("That folder doesn't look like a WoW game folder. Pick the one with Interface and WTF in it.")?;
    let info = paths::inspect(&picked_game).ok_or("That folder doesn't look like a WoW game folder.")?;
    let sess = open_session(&picked_game)?;
    let fresh = !companion::is_installed(&sess.fs);

    companion::install(&sess.fs, COMPANION_FILES).map_err(msg)?;

    let running = GameCheck::default().running(&picked_game);
    {
        let mut inner = state.inner.lock().unwrap();

        inner.game_running = running;

        if fresh && running
        {
            inner.settings.restart_game = true;
        }

        inner.settings.game = Some(picked_game);
        save_json("settings.json", &inner.settings)?;
        inner.session = Some(sess);

        let game = match info.flavour.as_str()
        {
            "_forever_" => "WoW Forever",
            "_classic_era_" => "WoW Classic Era",
            "_classic_" => "WoW Classic",
            "_anniversary_" => "WoW Anniversary",
            "_retail_" => "WoW Retail",
            other => other,
        };

        inner.log(format!("Using your {game} folder. Added the Companion add-on (Sterling Quest Sync Companion)."), "");
    }

    if let Some(tx) = state.rewatch.lock().unwrap().as_ref()
    {
        let _ = tx.send(());
    }

    emit_status(app);
    Ok(info)
}

/// Uses one of the folders `discover_games` found, by its position in that list.
#[tauri::command(async)]
fn use_found_game(app: AppHandle, state: State<'_, AppState>, index: usize) -> Result<GameFolder, String>
{
    let found = paths::discover();
    let game = found.get(index).ok_or("That folder isn't in the list any more.")?;

    set_game(&app, &state, game.path.clone())
}

/// Setup step 1: show a found folder without using it yet.
#[tauri::command(async)]
fn candidate_found(state: State<'_, AppState>, index: usize) -> Result<GameFolder, String>
{
    let found = paths::discover();
    let game = found.get(index).ok_or("That folder isn't in the list any more.")?;

    state.inner.lock().unwrap().candidate = Some(game.path.clone());
    Ok(game.clone())
}

/// Setup step 1: "Change..." opens a folder picker; the folder is shown, not used yet.
#[tauri::command(async)]
fn candidate_pick(app: AppHandle, state: State<'_, AppState>) -> Result<Option<GameFolder>, String>
{
    let Some(picked) = app.dialog().file().set_title("Pick your WoW game folder (the one with Interface and WTF)").blocking_pick_folder() else
    {
        return Ok(None);
    };

    let path = picked.into_path().map_err(|e| e.to_string())?;
    let picked_game = paths::normalise_pick(&path).ok_or("That folder doesn't look like a WoW game folder. Pick the one with Interface and WTF in it.")?;
    let info = paths::inspect(&picked_game).ok_or("That folder doesn't look like a WoW game folder.")?;

    state.inner.lock().unwrap().candidate = Some(picked_game);
    Ok(Some(info))
}

/// Setup step 1 Continue: use the shown folder, and install Sterling there if asked.
#[tauri::command(async)]
fn confirm_game(app: AppHandle, state: State<'_, AppState>, install_sterling: bool) -> Result<(), String>
{
    let candidate = state.inner.lock().unwrap().candidate.clone().ok_or("Pick a game folder first.")?;

    set_game(&app, &state, candidate)?;

    if install_sterling
    {
        install_sterling_now(&app)
            .map_err(|e| format!("Your folder is set, but Sterling Quest Sync couldn't be installed: {e}. You can try again from Home."))?;
    }

    Ok(())
}

/// Opens a folder picker on the Rust side; the page never sends a path.
#[tauri::command(async)]
fn pick_game_folder(app: AppHandle, state: State<'_, AppState>) -> Result<Option<GameFolder>, String>
{
    let Some(picked) = app.dialog().file().set_title("Pick your WoW game folder (the one with Interface and WTF)").blocking_pick_folder() else
    {
        return Ok(None);
    };

    let path = picked.into_path().map_err(|e| e.to_string())?;

    set_game(&app, &state, path).map(Some)
}

#[derive(Serialize)]
struct Joined
{
    recovery_code: Option<String>,
}

/// Saves the key and settings after this PC joins an account, then syncs.
fn finish_join(app: &AppHandle, app_state: &AppState, server: &str, key: &str, label: &str, recovery: Option<String>, how: &str) -> Result<Joined, String>
{
    app_state.keys.set(server, key).map_err(msg)?;
    {
        let mut inner = app_state.inner.lock().unwrap();

        inner.settings.joined = true;
        inner.settings.pc_label = label.to_string();
        inner.settings.server = server.to_string();
        inner.rejoin = false;
        inner.last_error = None;
        save_json("settings.json", &inner.settings)?;

        // A new account starts with a clean sync state on this PC.
        if let Some(sess) = inner.session.as_mut()
        {
            sess.state = engine::State { format: engine::STATE_FORMAT, ..Default::default() };

            let _ = engine::save_state(&sess.fs, &sess.state);
        }

        inner.log(how.to_string(), "good");
    }

    emit_status(app);

    let handle = app.clone();

    std::thread::spawn(move ||
    {
        let _ = run_sync(&handle);
    });

    Ok(Joined { recovery_code: recovery })
}

/// The server address from settings.
fn server_of(state: &AppState) -> String
{
    state.inner.lock().unwrap().settings.server.clone()
}

/// "Yes, this is my first PC", and "Start fresh" on the rejoin screen.
#[tauri::command(async)]
fn create_account(app: AppHandle, state: State<'_, AppState>, label: String, invite: Option<String>) -> Result<Joined, String>
{
    let label = clean_label(&label).ok_or("Give this PC a name.")?;
    let server = server_of(&state);
    let key = api::new_key();
    let client = Client::new(&server, &key).map_err(msg)?;
    let joined = client.join(&label, invite.as_deref().filter(|s| !s.trim().is_empty())).map_err(msg)?;

    finish_join(&app, &state, &server, &key, &label, joined.recovery_code, "Made your sync account.")
}

/// "No, I already use it on another PC".
#[tauri::command(async)]
fn link_this_pc(app: AppHandle, state: State<'_, AppState>, code: String, label: String) -> Result<Joined, String>
{
    let label = clean_label(&label).ok_or("Give this PC a name.")?;
    let server = server_of(&state);
    let key = api::new_key();

    Client::new(&server, &key).map_err(msg)?.redeem_link(&code, &label).map_err(msg)?;
    finish_join(&app, &state, &server, &key, &label, None, "Linked this PC to your account.")
}

#[tauri::command(async)]
fn recover_account(app: AppHandle, state: State<'_, AppState>, code: String, label: String) -> Result<Joined, String>
{
    let label = clean_label(&label).ok_or("Give this PC a name.")?;
    let server = server_of(&state);
    let key = api::new_key();
    let joined = Client::new(&server, &key).map_err(msg)?.recover(&code, &label).map_err(msg)?;

    finish_join(&app, &state, &server, &key, &label, joined.recovery_code, "Got your account back with the recovery code.")
}

/// Saves the recovery code as a .txt where the person picks, never inside the game folder.
#[tauri::command(async)]
fn save_recovery_txt(app: AppHandle, state: State<'_, AppState>, code: String) -> Result<bool, String>
{
    let typed = code.trim().to_uppercase();
    let ok = typed.starts_with("SQS-") && typed.len() <= 40 && typed.chars().all(|ch| ch.is_ascii_alphanumeric() || ch == '-');

    if !ok
    {
        return Err("That isn't a recovery code.".into());
    }

    let Some(path) = app.dialog().file().set_file_name("sterling-quest-sync-recovery.txt").add_filter("Text", &["txt"]).blocking_save_file() else
    {
        return Ok(false);
    };

    let path = path.into_path().map_err(|e| e.to_string())?;
    let text = format!
    (
        "Sterling Quest Sync Companion recovery code\r\n\r\n{typed}\r\n\r\nUse it when setting up the app again if no PC is left signed in.\r\nKeep it private: it lets someone use your sync account.\r\n"
    );

    let inner = state.inner.lock().unwrap();

    match &inner.session
    {
        Some(sess) => sess.fs.write_user_export(&path, text.as_bytes()).map_err(msg)?,
        None => fs::write(&path, text).map_err(|e| e.to_string())?,
    }

    Ok(true)
}

// ------------------------------------------------------------------ sync

/// Text with every character id ("Player-1234-0ABC1234") swapped for that character's name.
fn swap_guids(text: &str, name: &dyn Fn(&String) -> String) -> String
{
    let mut out = String::new();
    let mut rest = text;

    while let Some(at) = rest.find("Player-")
    {
        out.push_str(&rest[..at]);

        let tail = &rest[at..];
        let end = tail.char_indices().find(|(_, c)| !(c.is_ascii_alphanumeric() || *c == '-')).map(|(i, _)| i).unwrap_or(tail.len());
        let guid = tail[..end].to_string();

        out.push_str(&if sqs_core::formats::valid_guid(&guid) { name(&guid) } else { guid });
        rest = &tail[end..];
    }

    out.push_str(rest);
    out
}

/// The skip reasons as people can read them: character ids become names, repeats are counted.
fn skip_reasons(skipped: &[String], name: &dyn Fn(&String) -> String) -> Vec<String>
{
    let mut counted: Vec<(String, usize)> = Vec::new();

    for reason in skipped
    {
        let readable = swap_guids(reason, name);

        match counted.iter_mut().find(|(text, _)| *text == readable)
        {
            Some((_, count)) => *count += 1,
            None => counted.push((readable, 1)),
        }
    }

    counted.into_iter().map(|(text, count)| if count > 1 { format!("{text} (x{count})") } else { text }).collect()
}

/// Turns a sync report into the activity lines the window shows.
fn describe(inner: &mut Inner, report: &Report)
{
    let names = inner.names();
    let name = |g: &String| names.get(g).cloned().unwrap_or_else(|| "a character".into());
    let mut ups: Vec<String> = report.sent_to.iter().map(|(f, _)| name(f)).collect();

    ups.dedup();

    for uploaded in ups
    {
        inner.log(format!("Shared {uploaded}'s progress with your partners"), "good");
    }

    for (from_guid, to_guid) in &report.received_from
    {
        if from_guid == to_guid
        {
            inner.log(format!("Got {}'s copy from your other PC", name(from_guid)), "good");
        }

        else
        {
            inner.log(format!("Got {}'s latest copy for {}", name(from_guid), name(to_guid)), "good");
        }
    }

    for (from_guid, to_guid) in &report.stale
    {
        inner.log(format!("Skipped an older save of {} for {}: the server already has a newer one", name(from_guid), name(to_guid)), "");
    }

    for (_, _, who) in &report.auto_paired
    {
        inner.log(format!("Paired with {who} automatically"), "good");
    }

    for note in &report.auto_notes
    {
        inner.log(note.clone(), "");
    }

    for id in &report.ended
    {
        let who = inner.session.as_ref().and_then(|s| s.state.pairs.iter().find(|p| &p.id == id)).map(|p| p.partner_name.clone()).unwrap_or_default();

        inner.log(format!("Stopped syncing with {} (removed in game)", if who.is_empty() { "a partner".into() } else { who }), "");
    }

    let skipped = skip_reasons(&report.skipped, &name);

    if !skipped.is_empty() && skipped != inner.last_skipped
    {
        inner.log(format!("Skipped {} item(s) that didn't pass checks: {}", report.skipped.len(), skipped.join("; ")), "bad");
    }

    inner.last_skipped = skipped;

    if report.inbox_written && (!report.received_from.is_empty() || !report.ended.is_empty() || report.auto_offered > 0)
    {
        let how = if inner.game_running { "Type /reload in game to see it." } else { "You'll see it next time you log in." };

        inner.log(format!("Partners' progress is ready for the game. {how}"), "");
    }
}

const OFFLINE_GRACE_SECS: i64 = 180;

/// One full sync: read outboxes, talk to the server, write the inbox.
fn run_sync(app: &AppHandle) -> Result<Report, String>
{
    let state = app.state::<AppState>();
    let settings =
    {
        let mut inner = state.inner.lock().unwrap();

        if inner.busy
        {
            return Err("Already syncing.".into());
        }

        inner.busy = true;
        inner.settings.clone()
    };

    emit_status(app);
    // Every sync also picks up the owner's notices (one small request), on top of the launch and 6-hour checks,
    // and once an hour looks for a new app version, so a fix doesn't wait for the 6-hour check.
    check_notices(app);

    let due =
    {
        let state = app.state::<AppState>();
        let mut inner = state.inner.lock().unwrap();
        let due = inner.app_checked_at_sync.is_none_or(|at| at.elapsed() >= APP_CHECK_AT_SYNC);

        if due
        {
            inner.app_checked_at_sync = Some(Instant::now());
        }

        due
    };

    if due
    {
        let _ = check_app_update(app);
    }

    let res = (||
    {
        let client = state.client(&settings).map_err(Error::Other)?;
        let mut inner = state.inner.lock().unwrap();
        // Versions the add-on shows after a /reload.
        let app_info = AppInfo { version: VERSION.into(), latest: inner.app_latest.clone(), sterling_latest: inner.sterling_latest.clone() };
        let sess = inner.session.as_mut().ok_or_else(|| Error::Other("Pick your game folder first.".into()))?;
        let (outboxes, problems) = read_outboxes(&sess.fs);
        let mut report = engine::sync(&client, &sess.fs, &mut sess.state, &outboxes, Some(app_info), now());

        if let Ok(rep) = report.as_mut()
        {
            rep.skipped.extend(problems);
        }

        report
    })();

    {
        let mut inner = state.inner.lock().unwrap();

        inner.busy = false;

        match &res
        {
            Ok(report) =>
            {
                inner.last_error = None;
                inner.online = Some(true);
                inner.offline_since = None;
                describe(&mut inner, report);
            }

            Err(Error::Offline) =>
            {
                // Short blips fix themselves, so only say so after a few minutes without the server.
                let since = *inner.offline_since.get_or_insert(now());

                if now() - since >= OFFLINE_GRACE_SECS
                {
                    if inner.online != Some(false)
                    {
                        inner.log("Can't reach the sync server. Trying again soon.", "bad");
                    }

                    inner.online = Some(false);
                    inner.last_error = Some("Can't reach the sync server. Trying again soon.".into());
                }
            }

            Err(Error::Api { code, .. }) if code == "unknown_key" =>
            {
                inner.online = Some(true);
                inner.offline_since = None;
                inner.rejoin = true;
                inner.last_error = None;
            }

            Err(error) =>
            {
                inner.online = Some(true);
                inner.offline_since = None;

                let message = error.to_string();

                inner.log(format!("Sync failed: {message}"), "bad");
                inner.last_error = Some(message);
            }
        }
    }

    if let Ok(report) = &res
    {
        notify_favourites(app, report);
    }

    emit_status(app);
    res.map_err(msg)
}

#[tauri::command(async)]
fn sync_now(app: AppHandle) -> Result<Report, String>
{
    run_sync(&app)
}

#[tauri::command]
fn set_options
(
    app: AppHandle,
    state: State<'_, AppState>,
    auto_sync: Option<bool>,
    auto_update_sterling: Option<bool>,
    auto_update_app: Option<bool>,
    close_to_tray: Option<bool>,
    character: Option<String>,
    start_with_windows: Option<bool>,
    notify_favs: Option<bool>,
    close_note_seen: Option<bool>,
) -> Result<(), String>
{
    if let Some(value) = start_with_windows
    {
        set_autostart(value)?;
    }

    {
        let mut inner = state.inner.lock().unwrap();

        if let Some(value) = start_with_windows
        {
            inner.settings.start_with_windows = value;
            inner.settings.autostart_note = None;
        }

        if let Some(value) = notify_favs
        {
            inner.settings.notify_favs = value;
        }

        if let Some(version) = close_note_seen
        {
            inner.settings.close_note_seen = version;
        }

        if let Some(value) = auto_sync
        {
            inner.settings.auto_sync = value;
        }

        if let Some(value) = auto_update_sterling
        {
            inner.settings.auto_update_sterling = value;
        }

        if let Some(value) = close_to_tray
        {
            inner.settings.close_to_tray = value;
        }

        if let Some(value) = auto_update_app
        {
            inner.settings.auto_update_app = value;
        }

        if let Some(guid) = character.filter(|c| sqs_core::formats::valid_guid(c))
        {
            inner.settings.character = Some(guid);
        }

        save_json("settings.json", &inner.settings)?;
    }

    emit_status(&app);
    Ok(())
}

/// Star or unstar a partner of the selected character. Ok(false) means 5 are already starred.
#[tauri::command]
fn set_favorite(app: AppHandle, state: State<'_, AppState>, partner: String, on: bool) -> Result<bool, String>
{
    let ok =
    {
        let mut inner = state.inner.lock().unwrap();
        let me = inner.settings.character.clone().ok_or("Select a character first.")?;
        let sess = inner.session.as_mut().ok_or("Pick your game folder first.")?;

        engine::set_favorite(&sess.fs, &mut sess.state, &me, &partner, on, now()).map_err(msg)?
    };

    emit_status(&app);
    Ok(ok)
}

// ------------------------------------------------------------------ commands: pairing and account

/// Runs one server call with this PC's client, as window-ready errors.
fn with_client<T>(state: &AppState, work: impl FnOnce(&Client) -> sqs_core::Result<T>) -> Result<T, String>
{
    let settings = state.inner.lock().unwrap().settings.clone();
    let client = state.client(&settings)?;

    work(&client).map_err(msg)
}

/// The server lists only characters it has seen from the game. Pairing needs one.
fn ensure_listed(state: &AppState, guid: &str) -> Result<(), String>
{
    let found =
    {
        let inner = state.inner.lock().unwrap();

        inner.session.as_ref().and_then(|s| s.state.listed.get(guid).cloned())
    };

    match found
    {
        Some(_) => Ok(()),
        None => Err("Log in on that character once with Sterling Quest Sync and Sterling Quest Sync Companion enabled, then try again.".into()),
    }
}

#[tauri::command(async)]
fn make_pair_code(state: State<'_, AppState>, guid: String) -> Result<api::Code, String>
{
    ensure_listed(&state, &guid)?;
    with_client(&state, |c| c.pair_code(&guid))
}

#[tauri::command(async)]
fn open_codes(state: State<'_, AppState>) -> Result<Vec<api::OpenCode>, String>
{
    with_client(&state, |c| c.open_codes())
}

#[tauri::command(async)]
fn cancel_code(state: State<'_, AppState>, id: String) -> Result<(), String>
{
    with_client(&state, |c| c.cancel_code(&id))
}

#[tauri::command(async)]
fn peek_code(state: State<'_, AppState>, guid: String, code: String) -> Result<api::Peek, String>
{
    ensure_listed(&state, &guid)?;
    with_client(&state, |c| c.peek_code(&guid, &code))
}

#[tauri::command(async)]
fn redeem_code(app: AppHandle, state: State<'_, AppState>, guid: String, code: String, replace: bool) -> Result<api::Redeemed, String>
{
    let peek = with_client(&state, |c| c.peek_code(&guid, &code))?;
    let redeemed = with_client(&state, |c| c.redeem_code(&guid, &code, replace))?;

    state.inner.lock().unwrap().log(format!("Paired with {}", peek.partner.name), "good");

    let _ = run_sync(&app);

    Ok(redeemed)
}

#[tauri::command(async)]
fn end_pairing(app: AppHandle, state: State<'_, AppState>, id: String) -> Result<(), String>
{
    with_client(&state, |c| c.end_pairing(&id))?;

    let who =
    {
        let inner = state.inner.lock().unwrap();

        inner.session.as_ref().and_then(|s| s.state.pairs.iter().find(|p| p.id == id)).map(|p| p.partner_name.clone()).unwrap_or_default()
    };

    state.inner.lock().unwrap().log(format!("Stopped syncing with {who}"), "");

    let _ = run_sync(&app);

    Ok(())
}

#[tauri::command(async)]
fn account(state: State<'_, AppState>) -> Result<api::AccountInfo, String>
{
    with_client(&state, |c| c.account())
}

#[tauri::command(async)]
fn link_code(state: State<'_, AppState>) -> Result<api::Code, String>
{
    with_client(&state, |c| c.link_code())
}

#[tauri::command(async)]
fn new_recovery_code(state: State<'_, AppState>) -> Result<String, String>
{
    let new_code = with_client(&state, |new_code| new_code.new_recovery())?;

    state.inner.lock().unwrap().log("Made a new recovery code. The old one no longer works.", "");
    Ok(new_code)
}

#[tauri::command(async)]
fn remove_pc(state: State<'_, AppState>, id: String) -> Result<(), String>
{
    with_client(&state, |c| c.remove_pc(&id))?;
    state.inner.lock().unwrap().log("Removed a PC from your account.", "");
    Ok(())
}

/// "Delete my data". The confirm step lives in the page; the server does the rest.
#[tauri::command(async)]
fn delete_my_data(app: AppHandle, state: State<'_, AppState>) -> Result<(), String>
{
    with_client(&state, |c| c.delete_account())?;
    {
        let mut inner = state.inner.lock().unwrap();
        let server = inner.settings.server.clone();

        state.keys.delete(&server).map_err(msg)?;
        inner.settings.joined = false;
        save_json("settings.json", &inner.settings)?;

        if let Some(sess) = inner.session.as_mut()
        {
            sess.state = engine::State { format: engine::STATE_FORMAT, ..Default::default() };
            engine::save_state(&sess.fs, &sess.state).map_err(msg)?;
        }

        inner.log("Deleted your data from the server.", "");
    }

    emit_status(&app);
    Ok(())
}

// ------------------------------------------------------------------ Sterling updates

/// Checks for app and Sterling updates in the background, without bothering anyone.
fn check_sterling_quiet(app: &AppHandle) -> Option<String>
{
    let app_latest = updater::latest_tag(updater::APP_REPO);
    let state = app.state::<AppState>();

    if app_latest.is_some()
    {
        state.inner.lock().unwrap().app_latest = app_latest;
    }

    let rel = updater::latest_release().ok()?;
    let mut inner = state.inner.lock().unwrap();

    inner.sterling_latest = Some(rel.version.clone());
    inner.sterling_checked = Some(Instant::now());
    Some(rel.version)
}

/// Installs Sterling Quest Sync now, for the window's button.
fn install_sterling_now(app: &AppHandle) -> Result<String, String>
{
    install_sterling(app)
}

/// Downloads the latest Sterling Quest Sync and installs it into the game folder.
fn install_sterling(app: &AppHandle) -> Result<String, String>
{
    let state = app.state::<AppState>();
    let rel = updater::latest_release().map_err(msg)?;
    let bytes = updater::download(&rel).map_err(msg)?;
    {
        let mut inner = state.inner.lock().unwrap();
        let sess = inner.session.as_ref().ok_or("Pick your game folder first.")?;
        let fresh = updater::installed_version(sess.fs.game()).is_none();

        updater::install(&sess.fs, &bytes, &rel.version).map_err(msg)?;
        inner.sterling_latest = Some(rel.version.clone());

        if fresh
        {
            inner.settings.sterling_by_app = true;

            let _ = save_json("settings.json", &inner.settings);
        }

        let running = inner.game_running;

        if running && fresh
        {
            inner.settings.restart_game = true;

            let _ = save_json("settings.json", &inner.settings);
        }

        let hint = if running && fresh
        {
            " Exit WoW completely and start it again to load it. /reload won't pick up a new add-on."
        }

        else if running
        {
            " Type /reload in game to load it."
        }

        else
        {
            ""
        };

        inner.log(format!("{} Sterling Quest Sync {}.{}", if fresh { "Installed" } else { "Updated to" }, rel.version, hint), "good");
    }

    emit_status(app);
    Ok(rel.version)
}

#[tauri::command(async)]
fn update_sterling(app: AppHandle) -> Result<String, String>
{
    install_sterling(&app)
}

// ------------------------------------------------------------------ notices

/// Fetches the owner's notices (with /v1/meta: no key, nothing about this PC is sent).
/// Quiet: if the server can't be reached the last ones stay.
fn check_notices(app: &AppHandle)
{
    let server = app.state::<AppState>().inner.lock().unwrap().settings.server.clone();

    if let Ok(server_meta) = api::Client::meta(&server)
    {
        app.state::<AppState>().inner.lock().unwrap().notices = server_meta.notices;
    }
}

#[tauri::command]
fn dismiss_notice(app: AppHandle, state: State<'_, AppState>, id: String) -> Result<(), String>
{
    {
        let mut inner = state.inner.lock().unwrap();
        let list = &mut inner.settings.dismissed_notices;

        if !list.contains(&id)
        {
            list.push(id);

            // Only recent ones matter: a removed notice never comes back with the same id.
            let extra = list.len().saturating_sub(30);

            list.drain(..extra);
        }

        save_json("settings.json", &inner.settings).map_err(|e| e.to_string())?;
    }

    emit_status(&app);
    Ok(())
}

/// The notice's button. The link is checked again here, right before it opens.
#[tauri::command]
fn open_notice(state: State<'_, AppState>, id: String) -> Result<(), String>
{
    let link =
    {
        let inner = state.inner.lock().unwrap();

        inner.notices.iter().find(|n| n.id == id).and_then(|n| n.link.clone())
    };

    match link
    {
        Some(l) if notices::link_ok(&l) => open_url(&l),
        _ => Err("That link isn't available.".into()),
    }
}

/// An error with everything that caused it ("error sending request: connection refused"),
/// so a failed update says why instead of only which address it tried.
fn full_error(top: &dyn std::error::Error) -> String
{
    let mut out = top.to_string();
    let mut cur = top.source();

    while let Some(cause) = cur
    {
        let text = cause.to_string();

        if !text.is_empty() && !out.contains(&text)
        {
            out.push_str(": ");
            out.push_str(&text);
        }

        cur = cause.source();
    }

    out
}

// ------------------------------------------------------------------ app updates

/// Asks the update feed, and once more a few seconds later if that fails, so a short network
/// blip doesn't count as a failed update. The feed addresses are tried in order (tauri.conf.json):
/// this project's own server first, then GitHub.
async fn check_feed(updater: &tauri_plugin_updater::Updater) -> Result<Option<tauri_plugin_updater::Update>, String>
{
    match updater.check().await
    {
        Ok(found) => Ok(found),
        Err(_) =>
        {
            // Every caller runs this on its own thread (block_on), so a plain sleep holds up nothing else.
            std::thread::sleep(Duration::from_secs(3));
            updater.check().await.map_err(|e| full_error(&e))
        }
    }
}

/// Asks the update feed whether a newer, signed app is out. Signature checks happen on install.
fn check_app_update(app: &AppHandle) -> Option<String>
{
    let found = tauri::async_runtime::block_on(async
    {
        let updater = app.updater().ok()?;

        check_feed(&updater).await.ok().flatten().map(|x| x.version)
    });

    let state = app.state::<AppState>();

    state.inner.lock().unwrap().app_update = found.clone();
    found
}

/// Downloads the update, checks its signature against the key built into this app, and runs it.
/// The installer closes the app and starts the new version.
fn install_app_update(app: &AppHandle) -> Result<(), String>
{
    let set = |step: &str, percent: Option<u8>|
    {
        app.state::<AppState>().inner.lock().unwrap().updating = Some(UpdateStep { step: step.into(), percent });
        emit_status(app);
    };

    {
        let state = app.state::<AppState>();
        let mut inner = state.inner.lock().unwrap();

        if inner.updating.is_some()
        {
            return Err("Already updating.".into());
        }

        inner.updating = Some(UpdateStep { step: "Getting ready".into(), percent: None });
        inner.update_failed = None;
        inner.log("Updating the Companion...", "");
    }

    emit_status(app);

    let result = tauri::async_runtime::block_on(async
    {
        let updater = app.updater().map_err(|e| e.to_string())?;

        set("Finding the update", None);

        let update_found = check_feed(&updater).await?.ok_or("You already have the newest version.")?;
        let mut got: u64 = 0;
        let mut shown: Option<u8> = None;

        set("Downloading", Some(0));
        update_found
            .download_and_install
            (
                |chunk, total|
                {
                    got += chunk as u64;

                    if let Some(total_bytes) = total.filter(|total_bytes| *total_bytes > 0)
                    {
                        let pct = ((got * 100) / total_bytes).min(100) as u8;

                        if shown != Some(pct)
                        {
                            shown = Some(pct);
                            set("Downloading", Some(pct));
                        }
                    }
                },

                || set("Checking the signature and installing. The app reopens in a moment.", None),
            )
            .await
            .map_err(|e| full_error(&e))?;
        Ok::<(), String>(())
    });

    if let Err(error) = result
    {
        app.state::<AppState>().inner.lock().unwrap().updating = None;
        emit_status(app);

        return Err(error);
    }

    app.restart();
}

/// The Settings button: asks the update feed right now and says what it found.
#[tauri::command(async)]
fn check_for_app_update(app: AppHandle) -> Result<Option<String>, String>
{
    let handle = app.clone();
    let found = off_runtime(move ||
    {
        tauri::async_runtime::block_on(async
        {
            let updater = handle.updater().map_err(|e| e.to_string())?;

            check_feed(&updater).await.map(|x| x.map(|x| x.version))
        })
    })
    .map_err(|e| format!("Couldn't check for updates: {e}"))?;
    app.state::<AppState>().inner.lock().unwrap().app_update = found.clone();
    emit_status(&app);
    Ok(found)
}

/// Runs blocking update work on its own plain thread. Commands run inside the async runtime,
/// where `block_on` can't be used (it panicked there, so the Update button did nothing).
fn off_runtime<T: Send + 'static>(work: impl FnOnce() -> Result<T, String> + Send + 'static) -> Result<T, String>
{
    std::thread::spawn(work).join().unwrap_or_else(|_| Err("The update stopped unexpectedly. Details are in crash.log.".into()))
}

#[tauri::command(async)]
fn update_app(app: AppHandle) -> Result<(), String>
{
    let handle = app.clone();

    off_runtime(move || install_app_update(&handle)).map_err(|error|
    {
        {
            let state = app.state::<AppState>();
            let mut inner = state.inner.lock().unwrap();

            inner.updating = None;
            inner.update_failed = Some(error.clone());
            inner.log(format!("App update failed: {error}"), "bad");
        }

        emit_status(&app);
        error
    })
}

/// Opens the backups folder in the file manager (the page never sees the path).
#[tauri::command]
fn open_backups() -> Result<(), String>
{
    let path = app_data().join("backups");
    let _ = fs::create_dir_all(&path);
    #[cfg(windows)]
    let cmd = "explorer";
    #[cfg(target_os = "macos")]
    let cmd = "open";
    #[cfg(all(unix, not(target_os = "macos")))]
    let cmd = "xdg-open";

    std::process::Command::new(cmd).arg(path).spawn().map(|_| ()).map_err(|e| e.to_string())
}

/// Opens one of a few fixed web pages in the browser.
#[tauri::command]
fn open_page(which: String) -> Result<(), String>
{
    let url = match which.as_str()
    {
        "repo" => "https://github.com/SterlingAddons/SterlingQuestSync-App",
        "issues" => "https://github.com/SterlingAddons/SterlingQuestSync-App/issues",
        "privacy" => "https://github.com/SterlingAddons/SterlingQuestSync-Server#privacy",
        "help" => "https://github.com/SterlingAddons/SterlingQuestSync-App#troubleshooting",
        "download" => "https://github.com/SterlingAddons/SterlingQuestSync-App/releases/latest",
        _ => return Err("unknown page".into()),
    };

    open_url(url)
}

/// Opens one of Sterling's own pages in the browser. Callers pass fixed or checked addresses only.
fn open_url(url: &str) -> Result<(), String>
{
    #[cfg(windows)]
    let spawned = std::process::Command::new("rundll32").args(["url.dll,FileProtocolHandler", url]).spawn();
    #[cfg(target_os = "macos")]
    let spawned = std::process::Command::new("open").arg(url).spawn();
    #[cfg(all(unix, not(target_os = "macos")))]
    let spawned = std::process::Command::new("xdg-open").arg(url).spawn();

    spawned.map(|_| ()).map_err(|e| e.to_string())
}

#[tauri::command]
fn hide_to_tray(app: AppHandle)
{
    if let Some(window) = app.get_webview_window("main")
    {
        let _ = window.hide();
    }
}

/// The X button: hide to the tray, or quit, as set in Settings.
#[tauri::command]
fn close_window(app: AppHandle, state: State<'_, AppState>)
{
    if state.inner.lock().unwrap().settings.close_to_tray
    {
        hide_to_tray(app);
    }

    else
    {
        app.exit(0);
    }
}

#[tauri::command]
fn quit(app: AppHandle)
{
    app.exit(0);
}

// ------------------------------------------------------------------ background

/// Watches for saves and the game closing. Cheap: one file watcher, a process check every 15 s.
fn background(app: AppHandle)
{
    // Stay out of the game's way: this thread runs at background CPU and disk priority, and waits a
    // few seconds after launch so the window and the game don't fight for the PC at the same moment.
    #[cfg(windows)]
    unsafe
    {
        use windows_sys::Win32::System::Threading::{GetCurrentThread, SetThreadPriority, THREAD_MODE_BACKGROUND_BEGIN};
        SetThreadPriority(GetCurrentThread(), THREAD_MODE_BACKGROUND_BEGIN);
    }

    std::thread::sleep(Duration::from_secs(3));

    let (rewatch_tx, rewatch_rx) = channel::<()>();

    *app.state::<AppState>().rewatch.lock().unwrap() = Some(rewatch_tx);

    let mut game = GameCheck::default();
    let _ = check_sterling_quiet(&app);

    check_autostart(&app);

    let _ = check_app_update(&app);

    check_notices(&app);
    emit_status(&app);

    loop
    {
        let dirs: Vec<(String, PathBuf)> =
        {
            let state = app.state::<AppState>();
            let inner = state.inner.lock().unwrap();

            match &inner.session
            {
                Some(s) => paths::wtf_accounts(s.fs.game()).into_iter().map(|a| (a.clone(), paths::saved_variables(s.fs.game(), &a))).collect(),
                None => vec![],
            }
        };

        let (tx, rx) = channel::<String>();
        let _watcher = SaveWatcher::start(&dirs, tx).ok();
        let _ = maybe_sync(&app);
        let mut next_poll = Instant::now();

        loop
        {
            if rewatch_rx.try_recv().is_ok()
            {
                break;
            }

            match rx.recv_timeout(Duration::from_secs(5))
            {
                Ok(_) =>
                {
                    // The game writes several files at logout; wait for it to settle.
                    std::thread::sleep(Duration::from_secs(3));

                    while rx.try_recv().is_ok() {}

                    let _ = maybe_sync(&app);
                }

                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => std::thread::sleep(Duration::from_secs(5)),
            }

            if Instant::now() >= next_poll
            {
                next_poll = Instant::now() + Duration::from_secs(15);

                let (folder, auto_sterling, checked) =
                {
                    let state = app.state::<AppState>();
                    let inner = state.inner.lock().unwrap();
                    (inner.session.as_ref().map(|s| s.fs.game().to_path_buf()), inner.settings.auto_update_sterling, inner.sterling_checked)
                };

                if let Some(picked) = folder
                {
                    let running = game.running(&picked);
                    let found: Vec<String> = sqs_core::game::addon_managers(&mut game).into_iter().map(String::from).collect();
                    let managers_changed =
                    {
                        let state = app.state::<AppState>();
                        let mut inner = state.inner.lock().unwrap();
                        let changed = inner.managers != found;

                        inner.managers = found;
                        changed
                    };

                    if managers_changed
                    {
                        emit_status(&app);
                    }

                    let (was, cleared) =
                    {
                        let state = app.state::<AppState>();
                        let mut inner = state.inner.lock().unwrap();
                        // Game closed: its next start finds any newly added add-on.
                        let cleared = !running && inner.settings.restart_game;

                        if cleared
                        {
                            inner.settings.restart_game = false;

                            let _ = save_json("settings.json", &inner.settings);
                        }

                        (std::mem::replace(&mut inner.game_running, running), cleared)
                    };

                    if cleared
                    {
                        emit_status(&app);
                    }

                    if was != running
                    {
                        // Check the server when the game starts and right after it closes.
                        let _ = maybe_sync(&app);

                        emit_status(&app);
                    }

                    if checked.is_none_or(|c| c.elapsed() >= STERLING_CHECK_EVERY)
                    {
                        check_notices(&app);

                        let auto_app = app.state::<AppState>().inner.lock().unwrap().settings.auto_update_app;

                        if check_app_update(&app).is_some() && auto_app && !running
                        {
                            let _ = install_app_update(&app);
                        }

                        if let Some(latest) = check_sterling_quiet(&app)
                        {
                            let installed = updater::installed_version(&picked);
                            // Leave the add-on to WowUp or the CurseForge app when they look after it.
                            let ours =
                            {
                                let state = app.state::<AppState>();
                                let inner = state.inner.lock().unwrap();

                                inner.settings.sterling_by_app || inner.managers.is_empty()
                            };

                            if auto_sterling && ours && installed.as_deref().is_some_and(|i| updater::newer(&latest, i))
                            {
                                let _ = install_sterling(&app);
                            }
                        }

                        emit_status(&app);
                    }
                }
            }
        }
    }
}

/// Syncs on its own when auto sync is on and everything is set up.
fn maybe_sync(app: &AppHandle) -> Result<(), String>
{
    let ready =
    {
        let state = app.state::<AppState>();
        let inner = state.inner.lock().unwrap();

        inner.settings.auto_sync && inner.settings.joined && inner.session.is_some() && !inner.rejoin
    };

    if ready
    {
        run_sync(app)?;
    }

    else
    {
        emit_status(app);
    }

    Ok(())
}

/// Short notes shown once after an update, newest last. Keep each line plain and under ~80 characters.
const WHATS_NEW: &[(&str, &[&str])] = &
[
    (
        "0.1.30",
        &
        [
            "Fixed: your saves were skipped after WoW Forever stopped reporting the realm name. /reload once to share your progress.",
            "Activity now says why something was skipped at a sync, and Copy support details lists it too.",
            "New app versions now show on Home within the hour instead of up to 6 hours.",
        ],
    ),

    ("0.1.29", &["Test builds of Sterling Quest Sync (like 0.3.17.01) now show as installed and keep updating."]),
    ("0.1.28", &["If Start with Windows is turned off in Task Manager or Windows Settings, the app leaves it off and says so in Settings."]),
    ("0.1.27", &["Installing a new version over a much older one works again, without uninstalling first."]),
    (
        "0.1.26",
        &
        [
            "Updates are sturdier: the app asks Sterling's own server where the newest version is, with GitHub as a backup, and tries again if the first try fails.",
            "If an update still fails, Home says why and offers the download to install it yourself.",
        ],
    ),

    (
        "0.1.25",
        &["Important news about the app now shows as a banner on Home, only for the versions it's about.", "If an update fails, the message now says why."],
    ),

    (
        "0.1.24",
        &
        [
            "New names: the add-on is now Sterling Quest Sync, and this app is its Companion.",
            "The app notices when WoW didn't load the Companion add-on and tells you how to fix it.",
            "If WowUp or the CurseForge app keeps the add-on updated, the Companion leaves it to them.",
            "New in Settings: Start with Windows, favourite notifications and Copy details for help.",
            "Invite a friend who doesn't have the app from their row on the Partners page.",
        ],
    ),
];

/// Is the left version newer than the right one?
fn newer_version(left: &str, right: &str) -> bool
{
    let parts = |s: &str| s.split('.').map(|x| x.parse::<u32>().unwrap_or(0)).collect::<Vec<_>>();

    parts(left) > parts(right)
}

/// Notes for every version after the one last seen, up to this one. Nothing on a fresh install.
fn whats_new(seen: Option<&str>) -> Vec<(String, Vec<String>)>
{
    let Some(seen) = seen else { return Vec::new() };

    WHATS_NEW
        .iter()
        .filter(|(v, _)| newer_version(v, seen) && !newer_version(v, VERSION))
        .map(|(v, lines)| (v.to_string(), lines.iter().map(|l| l.to_string()).collect()))
        .collect()
}

#[cfg_attr(not(windows), allow(dead_code))]
const RUN_KEY: &str = "Software\\Microsoft\\Windows\\CurrentVersion\\Run";
/// Where Windows remembers startup entries switched off in Task Manager or Settings > Apps > Startup.
#[cfg_attr(not(windows), allow(dead_code))]
const APPROVED_KEY: &str = "Software\\Microsoft\\Windows\\CurrentVersion\\Explorer\\StartupApproved\\Run";
#[cfg_attr(not(windows), allow(dead_code))]
const RUN_NAME: &str = "SterlingQuestSync";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Autostart
{
    /// The entry is there and Windows runs it.
    On,
    /// The entry is there, but switched off in Task Manager or Windows Settings.
    OffInWindows,
    /// The entry is gone (removed by another program, or never added).
    Missing,
}

/// Windows' own rule: the first byte of the StartupApproved value is even (2, 6) when the entry is
/// allowed to run and odd (3, 7) when it was switched off. No value means allowed.
fn classify_autostart(run_present: bool, approved: Option<&[u8]>) -> Autostart
{
    if !run_present
    {
        return Autostart::Missing;
    }

    match approved.and_then(|b| b.first())
    {
        Some(b) if b & 1 == 1 => Autostart::OffInWindows,
        _ => Autostart::On,
    }
}

#[cfg(windows)]
fn wide(text: &str) -> Vec<u16>
{
    text.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Reads one registry value under HKCU. None when it isn't there.
#[cfg(windows)]
fn reg_read(key: &str, name: &str) -> Option<Vec<u8>>
{
    use windows_sys::Win32::System::Registry::{RegGetValueW, HKEY_CURRENT_USER, RRF_RT_ANY};

    let (wide_key, wide_name) = (wide(key), wide(name));
    let mut len: u32 = 0;
    let status =
        unsafe { RegGetValueW(HKEY_CURRENT_USER, wide_key.as_ptr(), wide_name.as_ptr(), RRF_RT_ANY, std::ptr::null_mut(), std::ptr::null_mut(), &mut len) };

    if status != 0
    {
        return None;
    }

    let mut buf = vec![0u8; len as usize];
    let status =
        unsafe { RegGetValueW(HKEY_CURRENT_USER, wide_key.as_ptr(), wide_name.as_ptr(), RRF_RT_ANY, std::ptr::null_mut(), buf.as_mut_ptr().cast(), &mut len) };

    if status != 0
    {
        return None;
    }

    buf.truncate(len as usize);
    Some(buf)
}

/// What Windows will do with the app's startup entry right now.
fn autostart_state() -> Autostart
{
    #[cfg(windows)]
    {
        let run = reg_read(RUN_KEY, RUN_NAME).is_some();
        let approved = reg_read(APPROVED_KEY, RUN_NAME);

        classify_autostart(run, approved.as_deref())
    }

    #[cfg(not(windows))]
    {
        Autostart::Missing
    }
}

/// Adds or removes the app from Windows startup, for this Windows user only (no admin needed).
/// It starts with --hidden, so it goes straight to the tray. Only ever called for the person's own
/// choice in Settings (or to keep the path current while it's on); never to undo a change made
/// outside the app.
fn set_autostart(on: bool) -> Result<(), String>
{
    #[cfg(windows)]
    {
        use windows_sys::Win32::System::Registry::{RegDeleteKeyValueW, RegSetKeyValueW, HKEY_CURRENT_USER, REG_BINARY, REG_SZ};

        let (key, name, approved) = (wide(RUN_KEY), wide(RUN_NAME), wide(APPROVED_KEY));

        if on
        {
            let exe = std::env::current_exe().map_err(|e| e.to_string())?;
            let cmd = wide(&format!("\"{}\" --hidden", exe.display()));
            let status = unsafe { RegSetKeyValueW(HKEY_CURRENT_USER, key.as_ptr(), name.as_ptr(), REG_SZ, cmd.as_ptr().cast(), (cmd.len() * 2) as u32) };

            if status != 0
            {
                return Err(format!("Couldn't add the app to Windows startup (error {status})."));
            }

            // The person just turned it on here, so a switch-off from Task Manager is lifted too
            // (the same value Task Manager writes when you click Enable).
            if autostart_state() == Autostart::OffInWindows
            {
                let enabled: [u8; 12] = [2, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0];

                unsafe { RegSetKeyValueW(HKEY_CURRENT_USER, approved.as_ptr(), name.as_ptr(), REG_BINARY, enabled.as_ptr().cast(), 12) };
            }
        }

        else
        {
            let status = unsafe { RegDeleteKeyValueW(HKEY_CURRENT_USER, key.as_ptr(), name.as_ptr()) };

            // 2 = not there, which is what we wanted anyway.
            if status != 0 && status != 2
            {
                return Err(format!("Couldn't remove the app from Windows startup (error {status})."));
            }

            unsafe { RegDeleteKeyValueW(HKEY_CURRENT_USER, approved.as_ptr(), name.as_ptr()) };
        }

        Ok(())
    }

    #[cfg(not(windows))]
    {
        let _ = on;

        Err("Start with Windows only works on Windows.".into())
    }
}

/// Start with Windows is on in the app but Windows says otherwise: someone switched it off in Task
/// Manager or Windows Settings, or another program removed it. Respect that: turn the app's setting
/// off too and say why in Settings. Never turns it back on. Runs once at launch (two registry reads).
fn check_autostart(app: &AppHandle)
{
    if !cfg!(windows)
    {
        return;
    }

    let changed =
    {
        let state = app.state::<AppState>();
        let mut inner = state.inner.lock().unwrap();

        if !inner.settings.start_with_windows
        {
            return;
        }

        let note = match autostart_state()
        {
            Autostart::On => return,
            Autostart::OffInWindows =>
            {
                "Turned off outside the app (in Task Manager or Windows Settings > Apps > Startup), so it stays off. Turn it back on here if you want it."
            }

            Autostart::Missing => "Removed from Windows startup by another program, so it stays off. Turn it back on here if you want it.",
        };

        inner.settings.start_with_windows = false;
        inner.settings.autostart_note = Some(note.into());
        inner.log("Start with Windows was turned off outside the app. Left off.", "");

        let _ = save_json("settings.json", &inner.settings);

        true
    };

    if changed
    {
        emit_status(app);
    }
}

/// A Windows notification for each favourite whose new progress just arrived, when turned on.
fn notify_favourites(app: &AppHandle, report: &Report)
{
    use tauri_plugin_notification::NotificationExt;

    let lines: Vec<String> =
    {
        let state = app.state::<AppState>();
        let inner = state.inner.lock().unwrap();

        if !inner.settings.notify_favs
        {
            return;
        }

        let Some(sess) = inner.session.as_ref() else { return };
        let names = inner.names();

        report
            .received_from
            .iter()
            .filter(|(from, to)| from != to && sess.state.favs.get(to).is_some_and(|f| f.list.contains(from)))
            .map(|(from, _)| names.get(from).cloned().unwrap_or_else(|| "A favourite".into()))
            .collect()
    };

    if lines.is_empty()
    {
        return;
    }

    let running = app.state::<AppState>().inner.lock().unwrap().game_running;
    let how = if running { "Type /reload in game to see it." } else { "You'll see it next time you log in." };
    let title = if lines.len() == 1 { format!("{}'s new progress arrived", lines[0]) } else { format!("New progress from {}", lines.join(", ")) };
    let _ = app.notification().builder().title(title).body(how).show();
}

/// Facts for a help request, with nothing that identifies anyone: character and partner names
/// become "Character 1" and "Partner A", no realms, GUIDs, codes, keys, account or PC names, and
/// the Windows user folder becomes %USERPROFILE%.
#[tauri::command]
fn support_details(state: State<'_, AppState>) -> String
{
    let status = build_status(&state);
    let inner = state.inner.lock().unwrap();
    let ago = |t: Option<i64>| match t
    {
        None => "never".to_string(),
        Some(time) =>
        {
            let minutes = (now() - time).max(0) / 60;

            if minutes < 1
            {
                "just now".into()
            }

            else if minutes < 120
            {
                format!("{minutes} min ago")
            }

            else if minutes < 48 * 60
            {
                format!("{} hrs ago", minutes / 60)
            }

            else
            {
                format!("{} days ago", minutes / 1440)
            }
        }
    };

    let yes_no = |b: bool| if b { "yes" } else { "no" };

    // Every name the app knows, mapped to a placeholder. Longest first so "Bran Ashvale" goes before "Bran".
    let mut names: Vec<(String, String)> = Vec::new();

    for (i, character) in status.characters.iter().enumerate()
    {
        names.push((character.name.clone(), format!("Character {}", i + 1)));

        if let Some(nick) = character.look.nick.clone()
        {
            names.push((nick, format!("Character {}", i + 1)));
        }
    }

    let mut letter = 0u8;
    let mut partner_label = |real_name: &str, names: &mut Vec<(String, String)>| -> String
    {
        if let Some((_, existing)) = names.iter().find(|(k, _)| k == real_name)
        {
            return existing.clone();
        }

        let made = format!("Partner {}", (b'A' + (letter % 26)) as char);

        letter += 1;
        names.push((real_name.to_string(), made.clone()));
        made
    };

    for character in &status.characters
    {
        for partner in &character.partners
        {
            let stand_in = partner_label(&partner.name, &mut names);

            if let Some(nick) = partner.look.nick.clone()
            {
                names.push((nick, stand_in));
            }
        }

        for waiting_partner in &character.unpaired
        {
            if let Some(name) = waiting_partner.name.as_deref().filter(|n| !n.is_empty())
            {
                let stand_in = partner_label(name, &mut names);

                if let Some(nick) = waiting_partner.look.nick.clone()
                {
                    names.push((nick, stand_in));
                }
            }
        }
    }

    for known_name in inner.names().values()
    {
        partner_label(known_name, &mut names);
    }

    names.retain(|(k, _)| k.chars().count() >= 2);
    names.sort_by_key(|(k, _)| std::cmp::Reverse(k.len()));

    let profile = std::env::var("USERPROFILE").or_else(|_| std::env::var("HOME")).unwrap_or_default();
    let pc = status.pc_label.clone();
    let clean = |text: &str| anonymise(text, &names, &profile, &pc);

    let mut out = String::new();
    let mut line = |k: &str, v: String| out.push_str(&format!("{k}: {v}\n"));

    line("Companion app", VERSION.to_string());
    line("Windows", windows_version());
    line("Game folder type", status.game.as_ref().map(|g| g.flavour.clone()).unwrap_or_else(|| "not set".into()));
    line("WoW running", yes_no(status.game_running).into());
    line("Restart of WoW pending", yes_no(status.restart_game).into());
    line("Companion add-on installed", yes_no(status.companion_installed).into());
    line("Companion add-on loaded by WoW", if status.helper_off { "NO, the game saved without it".into() } else { "yes, or not known yet".into() });
    line
    (
        "Sterling Quest Sync add-on",
        match (&status.sterling_version, &status.sterling_latest)
        {
            (None, _) => "not installed".into(),
            (Some(v), Some(l)) if l != v => format!("{v} (newest is {l})"),
            (Some(v), _) => format!("{v} (newest)"),
        },
    );

    line("Add-on managers on this PC", if inner.managers.is_empty() { "none seen".into() } else { inner.managers.join(", ") });
    line("Add-on installed by the Companion", yes_no(inner.settings.sterling_by_app).into());
    line
    (
        "Settings",
        format!
        (
            "sync automatically {}, keep add-on updated {}, update app automatically {}, close to tray {}, start with Windows {}, favourite notifications {}",
            yes_no(status.auto_sync),
            yes_no(status.auto_update_sterling),
            yes_no(status.auto_update_app),
            yes_no(status.close_to_tray),
            yes_no(status.start_with_windows),
            yes_no(status.notify_favs)
        ),
    );

    line
    (
        "Sync server",
        match status.online
        {
            Some(true) => "reachable".into(),
            Some(false) => "can't reach it".into(),
            None => "not checked yet".into(),
        },
    );

    line("Custom server", yes_no(inner.settings.server != api::DEFAULT_SERVER).into());
    line("Last sync", ago(status.last_sync));
    line("Last error", status.last_error.as_deref().map(|e| clean(e)).unwrap_or_else(|| "none".into()));
    line("Characters", status.characters.len().to_string());

    for (i, character) in status.characters.iter().enumerate()
    {
        let active = character.partners.iter().filter(|p| p.active).count();
        let waiting = character.partners.iter().filter(|p| p.active && p.last_copy.is_some() && !p.applied).count();
        let no_copy = character.partners.iter().filter(|p| p.active && p.last_copy.is_none()).count();
        let ended = character.partners.iter().filter(|p| !p.active).count();
        let app_no = character.unpaired.iter().filter(|u| u.app == "no").count();
        let app_yes = character.unpaired.len() - app_no;

        out.push_str(&format!
        (
            "  Character {}: level {}, add-on {}, last save {}, waiting to sync {}, paired {} (new copy not loaded yet {}, no copy yet {}, ended {}), not paired yet {} (have the app or unknown {}, no app {}), favourites {}\n",
            i + 1,
            character.look.level.map(|l| l.to_string()).unwrap_or_else(|| "?".into()),
            character.sterling.clone().unwrap_or_else(|| "?".into()),
            ago(character.saved),
            yes_no(character.ready),
            active, waiting, no_copy, ended,
            character.unpaired.len(), app_yes, app_no,
            character.favs.len()
        ));
    }

    out.push_str("Recent activity (newest first):\n");

    let recent: Vec<&Activity> = inner.activity.iter().take(12).collect();

    if recent.is_empty()
    {
        out.push_str("  none\n");
    }

    for event in recent
    {
        out.push_str(&format!("  {} {}\n", ago(Some(event.t)), clean(&event.text)));
    }

    out.push_str("Skipped at the last sync:\n");

    if inner.last_skipped.is_empty()
    {
        out.push_str("  nothing\n");
    }

    for reason in &inner.last_skipped
    {
        out.push_str(&format!("  {}\n", clean(reason)));
    }

    out.push_str("Crash log (newest first):\n");

    let crash = fs::read_to_string(app_data().join("crash.log")).unwrap_or_default();
    let tail: Vec<&str> = crash.lines().rev().take(5).collect();

    if tail.is_empty()
    {
        out.push_str("  none\n");
    }

    for line in tail
    {
        out.push_str(&format!("  {}\n", clean(line)));
    }

    out.push_str("(Names are replaced with Character 1, Partner A and so on. No codes, keys, account details or realms are included.)\n");
    out
}

/// Replaces the user folder, the PC's label and every known name (longest first) with placeholders.
fn anonymise(text: &str, names: &[(String, String)], profile: &str, pc: &str) -> String
{
    let mut cleaned = text.to_string();

    if profile.len() > 3
    {
        cleaned = cleaned.replace(profile, "%USERPROFILE%");
    }

    if pc.len() > 1
    {
        cleaned = cleaned.replace(pc, "[this PC]");
    }

    for (real, stand_in) in names
    {
        cleaned = replace_whole(&cleaned, real, stand_in);
    }

    cleaned
}

/// Replaces `from` only where it stands as a whole name, so "Ty" doesn't touch "Type".
fn replace_whole(text: &str, from: &str, to: &str) -> String
{
    let is_word = |c: char| c.is_alphanumeric();
    let mut out = String::with_capacity(text.len());
    let mut rest = text;

    while let Some(i) = rest.find(from)
    {
        let before = rest[..i].chars().next_back();
        let after = rest[i + from.len()..].chars().next();
        let whole = !before.is_some_and(is_word) && !after.is_some_and(is_word);

        out.push_str(&rest[..i]);
        out.push_str(if whole { to } else { from });
        rest = &rest[i + from.len()..];
    }

    out.push_str(rest);
    out
}

#[cfg(test)]
mod support_tests
{
    use super::anonymise;

    #[test]
    fn details_hide_names_paths_and_pc()
    {
        let mut names = vec!
        [
            ("Bran".to_string(), "Partner A".to_string()),
            ("Bran Ashvale".to_string(), "Partner A".to_string()),
            ("Kaelith Emberfall".to_string(), "Character 1".to_string()),
        ];

        names.sort_by_key(|(k, _)| std::cmp::Reverse(k.len()));

        let out = anonymise
        (
            "Got Bran Ashvale's latest copy for Kaelith Emberfall; Bran says hi; C:\\Users\\someone\\AppData on GAMING-PC",
            &names,
            "C:\\Users\\someone",
            "GAMING-PC",
        );

        assert_eq!(out, "Got Partner A's latest copy for Character 1; Partner A says hi; %USERPROFILE%\\AppData on [this PC]");

        let names = vec![("Ty".to_string(), "Partner B".to_string())];

        assert_eq!(anonymise("Type /reload. Ty paired.", &names, "", ""), "Type /reload. Partner B paired.");
    }
}

/// "Windows 11 (10.0.26100)" style text, best effort.
fn windows_version() -> String
{
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;

        if let Ok(output) = std::process::Command::new("cmd").args(["/C", "ver"]).creation_flags(0x0800_0000).output()
        {
            let version_text = String::from_utf8_lossy(&output.stdout).trim().to_string();

            if !version_text.is_empty()
            {
                return version_text;
            }
        }
    }

    std::env::consts::OS.to_string()
}

#[tauri::command]
fn dismiss_whats_new(app: AppHandle, state: State<'_, AppState>) -> Result<(), String>
{
    {
        let mut inner = state.inner.lock().unwrap();

        inner.settings.seen_version = Some(VERSION.into());
        save_json("settings.json", &inner.settings)?;
    }

    emit_status(&app);
    Ok(())
}

static PLACE_GEN: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Notes the window's spot after a move or resize, and saves it once things settle
/// (a drag sends many events). Minimized or hidden windows report odd spots, so those are skipped.
fn remember_placement(win: &tauri::Window)
{
    if win.is_minimized().unwrap_or(false) || !win.is_visible().unwrap_or(false)
    {
        return;
    }

    let app = win.app_handle().clone();
    {
        let state = app.state::<AppState>();
        let mut inner = state.inner.lock().unwrap();
        let mut place = inner.settings.window.unwrap_or_default();

        place.max = win.is_maximized().unwrap_or(false);

        if !place.max
        {
            if let (Ok(pos), Ok(size)) = (win.outer_position(), win.inner_size())
            {
                place = Placement { x: pos.x, y: pos.y, w: size.width, h: size.height, max: false };
            }
        }

        inner.settings.window = Some(place);
    }

    use std::sync::atomic::Ordering;

    let mine = PLACE_GEN.fetch_add(1, Ordering::SeqCst) + 1;

    std::thread::spawn(move ||
    {
        std::thread::sleep(Duration::from_millis(800));

        if PLACE_GEN.load(Ordering::SeqCst) == mine
        {
            let state = app.state::<AppState>();
            let inner = state.inner.lock().unwrap();
            let _ = save_json("settings.json", &inner.settings);
        }
    });
}

/// Puts the window back where it was, if that spot is still on a connected screen.
fn restore_placement(app: &AppHandle)
{
    let Some(win) = app.get_webview_window("main") else { return };
    let saved = app.state::<AppState>().inner.lock().unwrap().settings.window;

    if let Some(place) = saved.filter(|p| p.w > 0 && p.h > 0)
    {
        // The title bar's middle has to land on a screen, or the window could be unreachable.
        let (center_x, center_y) = (place.x + place.w as i32 / 2, place.y + 20);
        let on_screen = win.available_monitors().unwrap_or_default().iter().any(|monitor|
        {
            let (mon_pos, mon_size) = (monitor.position(), monitor.size());

            center_x >= mon_pos.x && center_x < mon_pos.x + mon_size.width as i32 && center_y >= mon_pos.y && center_y < mon_pos.y + mon_size.height as i32
        });

        if on_screen
        {
            let _ = win.set_size(tauri::PhysicalSize::new(place.w, place.h));
            let _ = win.set_position(tauri::PhysicalPosition::new(place.x, place.y));
        }

        if place.max
        {
            let _ = win.maximize();
        }
    }
}

/// Brings the main window back from the tray.
fn show_main(app: &AppHandle)
{
    if let Some(window) = app.get_webview_window("main")
    {
        let _ = window.show();
        let _ = window.unminimize();
        let _ = window.set_focus();
    }
}

// ------------------------------------------------------------------ main

/// Appends one line to crash.log in the app's own data folder, so a silent failure leaves a reason.
/// Kept small: the file starts over past 64 KB.
fn crash_log(text: &str)
{
    use std::io::Write;

    let dir = app_data();
    let _ = fs::create_dir_all(&dir);
    let path = dir.join("crash.log");

    if fs::metadata(&path).map(|m| m.len() > 64 * 1024).unwrap_or(false)
    {
        let _ = fs::remove_file(&path);
    }

    if let Ok(mut log) = fs::OpenOptions::new().create(true).append(true).open(&path)
    {
        let _ = writeln!(log, "{} v{VERSION}: {}", now(), text.replace('\n', " "));
    }
}

/// The window couldn't open. Says why in a plain Windows message instead of quitting silently.
fn startup_failed(err: &str)
{
    let mut text = format!("Sterling Quest Sync Companion couldn't open.\n\n{err}\n\n");

    if err.to_lowercase().contains("webview")
    {
        text.push_str("This PC is missing Microsoft Edge WebView2, which the app needs to show its window. Download the free Evergreen installer from https://developer.microsoft.com/microsoft-edge/webview2/, run it, then open Sterling Quest Sync Companion again.\n\n");
    }

    text.push_str("The details are saved in %APPDATA%\\SterlingQuestSync\\crash.log. To get help, email that file to support@sterlingaddons.com or open an issue on GitHub.");
    #[cfg(windows)]
    {
        use windows_sys::Win32::UI::WindowsAndMessaging::{MessageBoxW, MB_ICONERROR, MB_OK};

        let wide = |s: &str| s.encode_utf16().chain(std::iter::once(0)).collect::<Vec<u16>>();
        let (wide_text, wide_caption) = (wide(&text), wide("Sterling Quest Sync Companion"));

        unsafe { MessageBoxW(std::ptr::null_mut(), wide_text.as_ptr(), wide_caption.as_ptr(), MB_OK | MB_ICONERROR) };
    }

    #[cfg(not(windows))]
    eprintln!("{text}");
    std::process::exit(1);
}

/// Starts the app: tray, window, watchers and the background loop.
fn main()
{
    api::set_app_version(VERSION);

    let default_hook = std::panic::take_hook();

    std::panic::set_hook(Box::new(move |info|
    {
        crash_log(&format!("crashed: {info}"));
        default_hook(info);
    }));

    let settings: Settings = fs::read_to_string(app_data().join("settings.json")).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default();
    let session = settings.game.as_ref().and_then(|g| open_session(g).ok());

    if let Some(open_session) = &session
    {
        // Keep the companion's own files current; Inbox.lua is left alone.
        let _ = companion::install(&open_session.fs, COMPANION_FILES);
    }

    let state = AppState
    {
        inner: Mutex::new(Inner
        {
            settings,
            session,
            game_running: false,
            last_error: None,
            online: None,
            rejoin: false,
            activity: load_json("activity.json"),
            sterling_latest: None,
            app_latest: None,
            app_update: None,
            sterling_checked: None,
            busy: false,
            updating: None,
            candidate: None,
            offline_since: None,
            notices: Vec::new(),
            update_failed: None,
            managers: Vec::new(),
            last_skipped: Vec::new(),
            app_checked_at_sync: None,
        }),

        keys: Arc::new(SystemStore),
        rewatch: Mutex::new(None),
    };

    tauri::Builder::default()
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| show_main(app)))
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_updater::Builder::new().build())
        .plugin(tauri_plugin_notification::init())
        .manage(state)
        .on_window_event(|window, event|
        {
            if matches!(event, tauri::WindowEvent::Moved(_) | tauri::WindowEvent::Resized(_))
            {
                remember_placement(window);
            }

            // Alt+F4 and other ways of closing follow the same setting as the X button.
            if let tauri::WindowEvent::CloseRequested { api, .. } = event
            {
                let to_tray = window.app_handle().state::<AppState>().inner.lock().unwrap().settings.close_to_tray;

                if to_tray
                {
                    api.prevent_close();

                    let _ = window.hide();
                }
            }
        })
        .setup(|app|
        {
            // The window starts hidden so it can be put back in its old spot before anyone sees it.
            restore_placement(app.handle());

            // Started by Windows at login (--hidden): stay in the tray until opened.
            let hidden = std::env::args().any(|a| a == "--hidden");

            if !hidden
            {
                if let Some(window) = app.get_webview_window("main")
                {
                    let _ = window.show();
                    let _ = window.set_focus();
                }
            }

            {
                let state = app.state::<AppState>();
                let mut inner = state.inner.lock().unwrap();

                // Keep the startup entry pointing at this copy of the app, but only while Windows still
                // has it switched on: a startup entry turned off or removed outside the app stays off.
                if inner.settings.start_with_windows && autostart_state() == Autostart::On
                {
                    let _ = set_autostart(true);
                }

                // A fresh install has nothing to tell about; later updates show their notes once.
                if inner.settings.seen_version.is_none()
                {
                    // Already set up before this setting existed (0.1.23 or older): show 0.1.24's notes.
                    let seen = if inner.settings.joined { "0.1.23" } else { VERSION };

                    inner.settings.seen_version = Some(seen.into());

                    let _ = save_json("settings.json", &inner.settings);
                }
            }

            let open = MenuItem::with_id(app, "open", "Open Sterling Quest Companion", true, None::<&str>)?;
            let sync = MenuItem::with_id(app, "sync", "Sync now", true, None::<&str>)?;
            let quit = MenuItem::with_id(app, "quit", "Quit", true, None::<&str>)?;
            let menu = Menu::with_items(app, &[&open, &sync, &quit])?;
            let mut tray = TrayIconBuilder::with_id("main").tooltip(format!("Sterling Quest Companion v{VERSION}")).menu(&menu).show_menu_on_left_click(false);

            if let Some(icon) = app.default_window_icon()
            {
                tray = tray.icon(icon.clone());
            }

            tray.on_menu_event(|app, e| match e.id.as_ref()
            {
                "open" => show_main(app),
                "sync" =>
                {
                    let app_handle = app.clone();

                    std::thread::spawn(move ||
                    {
                        let _ = run_sync(&app_handle);
                    });
                }

                "quit" => app.exit(0),
                _ => {}
            })
            .on_tray_icon_event(|tray, event|
            {
                if let TrayIconEvent::Click { button: MouseButton::Left, button_state: MouseButtonState::Up, .. } = event
                {
                    show_main(tray.app_handle());
                }
            })
            .build(app)?;

            let handle = app.handle().clone();

            std::thread::spawn(move || background(handle));
            Ok(())
        })
        .invoke_handler(tauri::generate_handler!
        [
            status,
            set_favorite,
            discover_games,
            use_found_game,
            pick_game_folder,
            candidate_found,
            candidate_pick,
            confirm_game,
            create_account,
            link_this_pc,
            recover_account,
            save_recovery_txt,
            sync_now,
            set_options,
            make_pair_code,
            open_codes,
            cancel_code,
            peek_code,
            redeem_code,
            end_pairing,
            account,
            link_code,
            new_recovery_code,
            remove_pc,
            delete_my_data,
            update_sterling,
            update_app,
            check_for_app_update,
            support_details,
            dismiss_whats_new,
            dismiss_notice,
            open_notice,
            open_backups,
            open_page,
            hide_to_tray,
            close_window,
            quit,
        ])
        .run(tauri::generate_context!())
        .unwrap_or_else(|error|
        {
            crash_log(&format!("couldn't start: {error}"));
            startup_failed(&error.to_string());
        });
}

#[cfg(test)]
mod tests
{
    use super::*;

    /// Installed copies find each other, and find updates, through these values. Changing any of them
    /// strands people on an old version (0.1.24's publisher change broke installing over 0.1.23, and
    /// the repo move broke 0.1.23's update check). If one really must change, the installer and the
    /// update feed need a plan first; see "Sterling Quest Sync - Updates, Notices and Deploys".
    #[test]
    fn skip_reasons_name_characters_and_count_repeats()
    {
        let name = |guid: &String| if guid == "Player-6800-0000A11C" { "Kaelith".to_string() } else { "a character".to_string() };
        let skipped = vec!
        [
            "Player-6800-0000A11C: bad realm name".to_string(),
            "Player-6800-0000A11C: bad realm name".into(),
            "Player-1-0000BEEF: bad self copy".into(),
            "outbox file too large".into(),
        ];

        assert_eq!(skip_reasons(&skipped, &name), vec!["Kaelith: bad realm name (x2)", "a character: bad self copy", "outbox file too large"]);
    }

    #[test]
    fn startup_state_follows_windows()
    {
        assert_eq!(classify_autostart(true, None), Autostart::On);
        assert_eq!(classify_autostart(true, Some(&[2, 0, 0, 0])), Autostart::On);
        assert_eq!(classify_autostart(true, Some(&[6, 0, 0, 0])), Autostart::On);
        assert_eq!(classify_autostart(true, Some(&[3, 0, 0, 0, 1, 2])), Autostart::OffInWindows);
        assert_eq!(classify_autostart(true, Some(&[7])), Autostart::OffInWindows);
        assert_eq!(classify_autostart(false, None), Autostart::Missing);
        assert_eq!(classify_autostart(false, Some(&[2])), Autostart::Missing);
    }

    /// Every released version has its release notes before it's tagged (docs/release-notes/v<version>.md).
    /// Internal builds never get a tag, so they don't need notes yet.
    #[test]
    fn release_notes_exist_for_this_version()
    {
        if PACKAGE_VERSION.contains("-dev.")
        {
            return;
        }

        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(format!("../docs/release-notes/v{VERSION}.md"));
        let text = std::fs::read_to_string(&path).unwrap_or_default();

        assert!(text.contains("## What's new"), "write {} before tagging this version", path.display());
    }

    /// An internal build shows as the release it started from plus a 4th part; a release shows as is.
    #[test]
    fn shown_version_follows_the_versioning_rule()
    {
        match PACKAGE_VERSION.split_once("-dev.")
        {
            Some((release, build)) =>
            {
                let parts: Vec<u32> = release.split('.').map(|part| part.parse().unwrap()).collect();

                assert_eq!(VERSION, format!("{}.{}.{}.{:02}", parts[0], parts[1], parts[2] - 1, build.parse::<u32>().unwrap()));
            }

            None => assert_eq!(VERSION, PACKAGE_VERSION),
        }

        assert!(sqs_core::formats::valid_version(VERSION) && sqs_core::updater::valid_version(VERSION));
    }

    #[test]
    fn install_identity_never_changes()
    {
        let conf: serde_json::Value = serde_json::from_str(include_str!("../tauri.conf.json")).unwrap();

        assert_eq!(conf["productName"], "Sterling Quest Sync", "the install folder and Windows uninstall entry");
        assert_eq!(conf["identifier"], "com.sterlingaddons.questsync", "the app's identity on Windows");
        assert_eq!(conf["bundle"]["windows"]["nsis"]["installMode"], "currentUser", "where it installs");
        assert_eq!(conf["bundle"]["windows"]["nsis"]["template"], "installer.nsi", "the installer that finds older installs");

        let feeds = conf["plugins"]["updater"]["endpoints"].as_array().unwrap();

        assert_eq!(feeds[0], "https://sync.sterlingaddons.com/update/latest.json", "the update address this project controls");
        assert!(feeds.iter().any(|f| f == "https://github.com/SterlingAddons/SterlingQuestSync-App/releases/latest/download/latest.json"));

        let nsi = include_str!("../installer.nsi");

        assert!(nsi.contains("Call SterlingInstallDirFallback"), "the old-install fix in installer.nsi");
    }

    #[test]
    fn labels_are_cleaned()
    {
        assert_eq!(clean_label("  DESKTOP-KAEL  ").as_deref(), Some("DESKTOP-KAEL"));
        assert_eq!(clean_label("<script>").as_deref(), Some("script"));
        assert_eq!(clean_label("---"), None);
        assert_eq!(clean_label(&"a".repeat(60)).unwrap().len(), 40);
    }
}
