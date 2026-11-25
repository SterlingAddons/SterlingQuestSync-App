//! The two files shared with the game.
//!
//! Outbox (game to app): `WTF/Account/<ACCOUNT>/SavedVariables/SterlingQuestSync.lua`, written by the
//! game at logout or /reload from the companion add-on's SavedVariables `SterlingQuestSyncOut`.
//! The app only reads it, with a strict parser, and validates every field.
//!
//! Inbox (app to game): `Interface/AddOns/SterlingQuestSync/Inbox.lua`. It is Lua code, so it is
//! built from a fixed template and every value is allowlisted. No names or free text, ever.
//!
//! Format version 1. Field names here are the contract for the Sterling side of the work.

use std::collections::BTreeMap;

use regex::Regex;
use serde::{Deserialize, Serialize};
use std::sync::LazyLock;

use crate::lua::{parse_saved_variables, Table, Value};
use crate::{Error, Result};

pub const FORMAT: i64 = 1;
pub const MAX_PACKET: usize = 64 * 1024;

pub static RE_GUID: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^Player-\d{1,5}-[0-9A-F]{8}$").unwrap());
pub static RE_PACKET: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[A-Za-z0-9()]+$").unwrap());
pub static RE_PAIR_KEY: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[A-Z2-9]{16}$").unwrap());
/// A pairing code as the server hands it out: XXXX-XXXX-XXXX from its 30-letter alphabet.
pub static RE_PAIR_CODE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[A-HJKMNP-TV-Z2-9]{4}-[A-HJKMNP-TV-Z2-9]{4}-[A-HJKMNP-TV-Z2-9]{4}$").unwrap());
static RE_NAME: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^\p{L}[\p{L}' -]{0,23}$").unwrap());
static RE_REALM: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[\p{L}\p{N}][\p{L}\p{N}' ()-]{0,63}$").unwrap());

/// A realm name with anything the server wouldn't take dropped (the game can use other punctuation),
/// spaces tidied and cut to 64 characters. The realm is only shown, so a cleaned one is fine.
fn clean_realm(raw: &str) -> String
{
    let kept: String = raw.chars().filter(|c| c.is_alphanumeric() || " '()-".contains(*c)).collect();
    let tidy = kept.split_whitespace().collect::<Vec<_>>().join(" ");
    let start = tidy.trim_start_matches(|c: char| !c.is_alphanumeric());

    start.chars().take(64).collect()
}

/// A character GUID as the game writes them.
pub fn valid_guid(text: &str) -> bool
{
    RE_GUID.is_match(text)
}

/// A quest packet: LibDeflate's print alphabet, within the size cap.
pub fn valid_packet(text: &str) -> bool
{
    !text.is_empty() && text.len() <= MAX_PACKET && RE_PACKET.is_match(text)
}

// ------------------------------------------------------------------ outbox

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PartnerOut
{
    pub guid: String,
    /// Full copy for this partner, or None when the partner was removed in game.
    pub packet: Option<String>,
    /// Change number for the partner entry (two-way edits, remove wins).
    pub change: i64,
    pub removed: bool,
    /// Display name, if Sterling wrote one. Shown only in this PC's window; never uploaded.
    #[serde(default)]
    pub name: Option<String>,
    /// Whether this partner has Sterling Quest Sync, as their Sterling last told ours in game.
    #[serde(default)]
    pub app: PartnerApp,
    #[serde(default)]
    pub look: Look,
}

/// Level, class and race for the app's window only. Never uploaded or written to the inbox.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Look
{
    pub level: Option<i64>,
    /// The game's class token, e.g. "DRUID".
    pub class: Option<String>,
    pub race: Option<String>,
    /// The person's first name or nickname as set in Sterling ("Bran"), shown above the character name.
    pub nick: Option<String>,
    /// GetServerTime() of the last live sync with this partner in game.
    pub seen: Option<i64>,
}

const CLASSES: [&str; 13] =
    ["WARRIOR", "PALADIN", "HUNTER", "ROGUE", "PRIEST", "SHAMAN", "MAGE", "WARLOCK", "DRUID", "DEATHKNIGHT", "MONK", "DEMONHUNTER", "EVOKER"];

/// A character's look (class, race, level and so on), with every field checked.
fn look(table: &Table) -> Look
{
    Look
    {
        level: int(table, "level").filter(|l| (1..=100).contains(l)),
        class: text(table, "class").filter(|c| CLASSES.contains(c)).map(String::from),
        race: text(table, "race").filter(|r| RE_NAME.is_match(r)).map(String::from),
        nick: text(table, "nick").filter(|r| RE_NAME.is_match(r)).map(String::from),
        seen: int(table, "seen").filter(|s| valid_time(*s)),
    }
}

/// `app = "0.1.9"` (has the app), `app = false` (seen in game, no app), or missing (not seen since the update).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub enum PartnerApp
{
    #[default]
    Unknown,
    No,
    Has(String),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CharacterOut
{
    pub guid: String,
    pub name: String,
    pub realm: String,
    /// GetServerTime() at the save.
    pub saved: i64,
    pub proto: i64,
    /// Copy for your own other PCs (quest order, nicknames).
    pub self_packet: Option<String>,
    pub partners: Vec<PartnerOut>,
    /// Last server number Sterling applied per sender GUID (lets the app know what arrived).
    pub applied: BTreeMap<String, i64>,
    /// Auto-pairing: codes this character received in game from a Sterling partner, by sender GUID.
    #[serde(default)]
    pub codes: BTreeMap<String, String>,
    #[serde(default)]
    pub look: Look,
    /// Sterling's own version, for the window.
    #[serde(default)]
    pub sterling: Option<String>,
    /// Favourite partners as Sterling last saved them, with when they last changed.
    #[serde(default)]
    pub favs: Option<Favs>,
}

pub const MAX_FAVS: usize = 5;

/// Favourite partners (up to 5, in order) and when the list last changed. Whichever side changed it
/// last wins: Sterling through the outbox, the app through the inbox.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Favs
{
    pub list: Vec<String>,
    pub change: i64,
}

impl Favs
{
    /// Keeps only good GUIDs, no repeats, at most 5.
    pub fn clean(list: impl IntoIterator<Item = String>, change: i64) -> Favs
    {
        let mut out: Vec<String> = Vec::new();

        for guid in list
        {
            if valid_guid(&guid) && !out.contains(&guid) && out.len() < MAX_FAVS
            {
                out.push(guid);
            }
        }

        Favs { list: out, change }
    }
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct Outbox
{
    pub characters: Vec<CharacterOut>,
    /// Entries dropped by validation, as short reasons (never their content).
    pub skipped: Vec<String>,
}

/// A whole number field from a Lua table.
fn int(table: &Table, key: &str) -> Option<i64>
{
    table.get(key).and_then(Value::as_i64)
}

/// A text field from a Lua table.
fn text<'a>(table: &'a Table, key: &str) -> Option<&'a str>
{
    table.get(key).and_then(Value::as_str)
}

/// A Unix time in a sane range.
fn valid_time(time: i64) -> bool
{
    (1_000_000_000..4_000_000_000).contains(&time)
}

/// Reads the outbox file. Anything that doesn't validate is skipped, never guessed at.
pub fn parse_outbox(file_text: &str) -> Result<Outbox>
{
    let vars = parse_saved_variables(file_text)?;
    let root = match vars.get("SterlingQuestSyncOut")
    {
        Some(Value::Table(t)) => t,
        _ => return Ok(Outbox::default()),
    };

    if int(root, "format") != Some(FORMAT)
    {
        return Err(Error::Format("outbox format is not 1".into()));
    }

    let mut out = Outbox::default();
    let chars = match root.get("characters").and_then(Value::as_table)
    {
        Some(c) => c,
        None => return Ok(out),
    };

    for (guid, value) in &chars.map
    {
        let Some(fields) = value.as_table() else { continue };

        if !valid_guid(guid)
        {
            out.skipped.push("character with a bad GUID".into());
            continue;
        }

        let name = text(fields, "name").unwrap_or("");
        let realm_text = clean_realm(text(fields, "realm").unwrap_or(""));
        let realm = realm_text.as_str();
        let saved = int(fields, "saved").unwrap_or(0);
        let proto = int(fields, "proto").unwrap_or(0);
        let problem = if !RE_NAME.is_match(name)
        {
            "bad character name"
        }

        // Some game versions report no realm at all (WoW Forever since Nov 2026); that's fine, the sync picks one.
        else if !realm.is_empty() && !RE_REALM.is_match(realm)
        {
            "bad realm name"
        }

        else if !valid_time(saved)
        {
            "bad save time"
        }

        else if !(1..=10_000).contains(&proto)
        {
            "unknown protocol"
        }

        else
        {
            ""
        };

        if !problem.is_empty()
        {
            out.skipped.push(format!("{guid}: {problem}"));
            continue;
        }

        let self_packet = match text(fields, "self")
        {
            Some(p) if valid_packet(p) => Some(p.to_string()),
            Some(_) =>
            {
                out.skipped.push(format!("{guid}: bad self copy"));
                None
            }

            None => None,
        };

        let mut partners = Vec::new();

        if let Some(partner_table) = fields.get("partners").and_then(Value::as_table)
        {
            for (partner_guid, partner_value) in &partner_table.map
            {
                let Some(partner_fields) = partner_value.as_table() else { continue };

                if !valid_guid(partner_guid) || partner_guid == guid
                {
                    out.skipped.push(format!("{guid}: partner with a bad GUID"));
                    continue;
                }

                let removed = partner_fields.get("removed").and_then(Value::as_bool).unwrap_or(false);
                let change = int(partner_fields, "change").unwrap_or(0);

                if change < 0
                {
                    out.skipped.push(format!("{guid}: partner with a bad change number"));
                    continue;
                }

                let packet = match text(partner_fields, "packet")
                {
                    Some(s) if valid_packet(s) => Some(s.to_string()),
                    Some(_) =>
                    {
                        out.skipped.push(format!("{guid}: bad partner copy"));
                        None
                    }

                    None => None,
                };

                let name = text(partner_fields, "name").filter(|n| RE_NAME.is_match(n)).map(String::from);
                let app = match partner_fields.get("app")
                {
                    Some(v) if v.as_bool() == Some(false) => PartnerApp::No,
                    Some(v) => match v.as_str()
                    {
                        Some(ver) if valid_version(ver) => PartnerApp::Has(ver.to_string()),
                        _ =>
                        {
                            out.skipped.push(format!("{guid}: partner with a bad app version"));
                            PartnerApp::Unknown
                        }
                    },

                    None => PartnerApp::Unknown,
                };

                partners.push(PartnerOut
                {
                    guid: partner_guid.clone(),
                    packet: if removed { None } else { packet },
                    change,
                    removed,
                    name,
                    app,
                    look: look(partner_fields),
                });
            }
        }

        let mut applied = BTreeMap::new();

        if let Some(applied_table) = fields.get("applied").and_then(Value::as_table)
        {
            for (key, value) in &applied_table.map
            {
                if let Some(number) = value.as_i64()
                {
                    if valid_guid(key) && number >= 0
                    {
                        applied.insert(key.clone(), number);
                    }
                }
            }
        }

        let mut codes = BTreeMap::new();

        if let Some(code_table) = fields.get("codes").and_then(Value::as_table)
        {
            for (from, value) in &code_table.map
            {
                let code = value.as_table().and_then(|t| text(t, "code")).or_else(|| value.as_str());

                if let Some(code) = code
                {
                    if valid_guid(from) && from != guid && RE_PAIR_CODE.is_match(code)
                    {
                        codes.insert(from.clone(), code.to_string());
                    }

                    else
                    {
                        out.skipped.push(format!("{guid}: bad pairing code entry"));
                    }
                }
            }
        }

        let favs = match (fields.get("favs").and_then(Value::as_table), int(fields, "favs_change"))
        {
            (Some(t), Some(ch)) if valid_time(ch) => Some(Favs::clean(t.list.iter().filter_map(|v| v.as_str().map(String::from)).filter(|g| g != guid), ch)),
            (Some(_), Some(_)) =>
            {
                out.skipped.push(format!("{guid}: favourites with a bad change time"));
                None
            }

            // Sterling leaves favs_change out until favourites were ever set: nothing to take yet.
            (Some(_), None) => None,
            _ => None,
        };

        out.characters.push(CharacterOut
        {
            favs,
            look: look(fields),
            sterling: text(fields, "sterling").filter(|v| valid_version(v)).map(String::from),
            codes,
            guid: guid.clone(),
            name: name.to_string(),
            realm: realm.to_string(),
            saved,
            proto,
            self_packet,
            partners,
            applied,
        });
    }

    Ok(out)
}

// ------------------------------------------------------------------ inbox

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Kind
{
    Partner,
    SelfCopy,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InPacket
{
    pub seq: i64,
    pub from: String,
    pub kind: Kind,
    pub time: i64,
    pub proto: i64,
    pub data: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum PairStatus
{
    Active,
    Ended,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InPair
{
    pub partner: String,
    pub my_key: Option<String>,
    pub partner_key: Option<String>,
    pub status: PairStatus,
    pub change: i64,
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct InChar
{
    pub packets: Vec<InPacket>,
    pub pairs: Vec<InPair>,
    /// Auto-pairing: codes for Sterling to hand to these partners in game.
    #[serde(default)]
    pub offers: Vec<InOffer>,
    /// Favourite partners set in the app, for Sterling to take when newer than its own.
    #[serde(default)]
    pub favs: Option<Favs>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InOffer
{
    pub partner: String,
    pub code: String,
    pub expires: i64,
}

impl Inbox
{
    pub fn set_offers(&mut self, to: &str, offers: Vec<InOffer>) -> bool
    {
        let character = self.chars.entry(to.to_string()).or_default();

        if character.offers == offers
        {
            return false;
        }

        character.offers = offers;
        true
    }

    pub fn set_favs(&mut self, to: &str, favs: Option<Favs>) -> bool
    {
        let entry = self.chars.entry(to.to_string()).or_default();

        if entry.favs == favs
        {
            return false;
        }

        entry.favs = favs;
        true
    }
}

/// What the app keeps for the inbox, by target character GUID.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct Inbox
{
    pub chars: BTreeMap<String, InChar>,
    /// Versions for the add-on to show ("app update available" after a /reload).
    #[serde(default)]
    pub app: Option<AppInfo>,
}

/// Version numbers only: digits and dots.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct AppInfo
{
    /// This app's version.
    pub version: String,
    /// Newest app release, if known.
    pub latest: Option<String>,
    /// Newest Sterling release, if known.
    pub sterling_latest: Option<String>,
}

static RE_VERSION: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^\d{1,4}\.\d{1,4}(\.\d{1,4}(\.\d{1,3})?)?$").unwrap());

/// A version like 0.1.25.
pub fn valid_version(version: &str) -> bool
{
    RE_VERSION.is_match(version)
}

impl Inbox
{
    /// Adds a packet, replacing an older one from the same sender and kind. Never removes otherwise.
    pub fn put_packet(&mut self, to: &str, packet: InPacket) -> bool
    {
        let character = self.chars.entry(to.to_string()).or_default();

        if let Some(old) = character.packets.iter_mut().find(|o| o.from == packet.from && o.kind == packet.kind)
        {
            if old.seq >= packet.seq
            {
                return false;
            }

            *old = packet;
        }

        else
        {
            character.packets.push(packet);
        }

        character.packets.sort_by_key(|x| x.seq);
        true
    }

    pub fn set_pairs(&mut self, to: &str, pairs: Vec<InPair>) -> bool
    {
        let character = self.chars.entry(to.to_string()).or_default();

        if character.pairs == pairs
        {
            return false;
        }

        character.pairs = pairs;
        true
    }
}

/// Checks every value against the allowlist before the inbox is written.
fn check_inbox(inbox: &Inbox) -> Result<()>
{
    let bad = |m: &str| Err(Error::Refused(format!("inbox value failed the allowlist: {m}")));

    if let Some(app_info) = &inbox.app
    {
        let ok = valid_version(&app_info.version) && [&app_info.latest, &app_info.sterling_latest].into_iter().flatten().all(|v| valid_version(v));

        if !ok
        {
            return bad("version");
        }
    }

    for (to, character) in &inbox.chars
    {
        if !valid_guid(to)
        {
            return bad("target GUID");
        }

        for in_packet in &character.packets
        {
            if !valid_guid(&in_packet.from)
                || in_packet.seq < 0
                || !valid_time(in_packet.time)
                || !(1..=10_000).contains(&in_packet.proto)
                || !valid_packet(&in_packet.data)
            {
                return bad("packet");
            }
        }

        for in_offer in &character.offers
        {
            if !valid_guid(&in_offer.partner) || !RE_PAIR_CODE.is_match(&in_offer.code) || !valid_time(in_offer.expires)
            {
                return bad("offer");
            }
        }

        if let Some(favs) = &character.favs
        {
            if favs.list.len() > MAX_FAVS || !favs.list.iter().all(|g| valid_guid(g)) || !valid_time(favs.change)
            {
                return bad("favourites");
            }
        }

        for in_pair in &character.pairs
        {
            if !valid_guid(&in_pair.partner) || in_pair.change < 0
            {
                return bad("pair");
            }

            for pair_key in [&in_pair.my_key, &in_pair.partner_key].into_iter().flatten()
            {
                if !RE_PAIR_KEY.is_match(pair_key)
                {
                    return bad("pair key");
                }
            }
        }
    }

    Ok(())
}

/// Builds Inbox.lua from the fixed template. Every value has already passed the allowlist,
/// and only digits, GUIDs, keys and packet characters ever reach the file.
pub fn render_inbox(inbox: &Inbox, written: i64) -> Result<String>
{
    check_inbox(inbox)?;

    let mut lua = String::new();

    lua.push_str("-- Written by the Sterling Quest Sync Companion app. Do not edit; it is replaced on every sync.\n");
    lua.push_str("SterlingQuestSync_Inbox = {\n");
    lua.push_str(&format!("  format = {},\n  written = {},\n", FORMAT, written));

    if let Some(app_info) = &inbox.app
    {
        let quoted = |x: &Option<String>| x.as_ref().map(|s| format!("\"{s}\"")).unwrap_or_else(|| "nil".into());

        lua.push_str(&format!
        (
            "  app = {{ version = \"{}\", latest = {}, sterling_latest = {} }},\n",
            app_info.version,
            quoted(&app_info.latest),
            quoted(&app_info.sterling_latest)
        ));
    }

    lua.push_str("  chars = {\n");

    for (to, character) in &inbox.chars
    {
        lua.push_str(&format!("    [\"{to}\"] = {{\n      packets = {{\n"));

        for packet in &character.packets
        {
            let kind = match packet.kind
            {
                Kind::Partner => "partner",
                Kind::SelfCopy => "self",
            };

            lua.push_str(&format!
            (
                "        {{ seq = {}, from = \"{}\", kind = \"{}\", time = {}, proto = {}, data = \"{}\" }},\n",
                packet.seq, packet.from, kind, packet.time, packet.proto, packet.data
            ));
        }

        lua.push_str("      },\n      pairs = {\n");

        for pair in &character.pairs
        {
            let status = match pair.status
            {
                PairStatus::Active => "active",
                PairStatus::Ended => "ended",
            };

            let key = |k: &Option<String>| match k
            {
                Some(v) => format!("\"{v}\""),
                None => "nil".to_string(),
            };

            lua.push_str(&format!
            (
                "        {{ partner = \"{}\", my_key = {}, partner_key = {}, status = \"{}\", change = {} }},\n",
                pair.partner,
                key(&pair.my_key),
                key(&pair.partner_key),
                status,
                pair.change
            ));
        }

        lua.push_str("      },\n      offers = {\n");

        for offer in &character.offers
        {
            lua.push_str(&format!("        {{ partner = \"{}\", code = \"{}\", expires = {} }},\n", offer.partner, offer.code, offer.expires));
        }

        lua.push_str("      },\n");

        if let Some(fav_list) = &character.favs
        {
            lua.push_str("      favs = {");

            for guid in &fav_list.list
            {
                lua.push_str(&format!(" \"{guid}\","));
            }

            lua.push_str(&format!(" }},\n      favs_change = {},\n", fav_list.change));
        }

        lua.push_str("    },\n");
    }

    lua.push_str("  },\n}\n");
    Ok(lua)
}

/// The companion add-on's TOC, installed by the app.
pub fn companion_toc() -> String
{
    [
        "## Interface: 16001, 11509",
        "## Title: Sterling Quest Sync Companion",
        "## Notes: Part of the optional Sterling Quest Sync Companion app. Passes quest progress between the add-on and the app. Keep this enabled while you use the app.",
        "## Author: Sterling",
        "## IconTexture: Interface\\AddOns\\SterlingQuestSync\\icon",
        "## Version: 1",
        "## SavedVariables: SterlingQuestSyncOut",
        "",
        "Inbox.lua",
        "Core.lua",
        "",
    ]
    .join("\n")
}

/// An inbox with nothing in it.
pub fn empty_inbox_lua() -> String
{
    render_inbox(&Inbox::default(), 1_000_000_000).unwrap()
}

#[cfg(test)]
mod tests
{
    use super::*;
    use crate::lua::parse_saved_variables;

    const ME: &str = "Player-6800-0000A11C";
    const BRAN: &str = "Player-6800-0A1B2C3D";

    fn sample_outbox() -> String
    {
        format!
        (
            r#"SterlingQuestSyncOut = {{
	["format"] = 1,
	["characters"] = {{
		["{ME}"] = {{
			["name"] = "Kaelith Emberfall",
			["realm"] = "CAMWOW US2 Realm 2 PvP",
			["saved"] = 1794770000,
			["proto"] = 6,
			["self"] = "SELFabc123",
			["partners"] = {{
				["{BRAN}"] = {{ ["packet"] = "STO1abc(def)", ["change"] = 3 }},
				["Player-6800-0000BEEF"] = {{ ["removed"] = true, ["change"] = 5 }},
				["Player-6800-00000BAD"] = {{ ["packet"] = "x\"]] os.exit() --", ["change"] = 1 }},
				["not-a-guid"] = {{ ["packet"] = "AAA", ["change"] = 1 }},
			}},
			["applied"] = {{ ["{BRAN}"] = 48213 }},
		}},
		["evil"] = {{ ["name"] = "X" }},
	}},
}}"#
        )
    }

    #[test]
    fn reads_and_validates_the_outbox()
    {
        let outbox = parse_outbox(&sample_outbox()).unwrap();

        assert_eq!(outbox.characters.len(), 1);

        let character = &outbox.characters[0];

        assert_eq!(character.name, "Kaelith Emberfall");
        assert_eq!(character.self_packet.as_deref(), Some("SELFabc123"));

        let bran = character.partners.iter().find(|p| p.guid == BRAN).unwrap();

        assert_eq!(bran.packet.as_deref(), Some("STO1abc(def)"));

        let removed = character.partners.iter().find(|p| p.guid == "Player-6800-0000BEEF").unwrap();

        assert!(removed.removed && removed.packet.is_none());

        let bad = character.partners.iter().find(|p| p.guid == "Player-6800-00000BAD").unwrap();

        assert!(bad.packet.is_none());
        assert!(!character.partners.iter().any(|p| p.guid == "not-a-guid"));
        assert_eq!(character.applied.get(BRAN), Some(&48213));
        assert!(outbox.skipped.len() >= 3);
    }

    #[test]
    fn odd_realm_names_are_cleaned_and_bad_entries_say_why()
    {
        assert_eq!(clean_realm("CAMWOW US2 Realm 2 PvP"), "CAMWOW US2 Realm 2 PvP");
        assert_eq!(clean_realm("[Forever] Doom:howl  (PvP)."), "Forever Doomhowl (PvP)");
        assert_eq!(clean_realm("::"), "");
        assert!(parse_outbox(&format!("SterlingQuestSyncOut = {{ [\"format\"] = 1, [\"characters\"] = {{ [\"{ME}\"] = {{ [\"name\"] = \"Kaelith Emberfall\", [\"realm\"] = \"\", [\"saved\"] = 1794770000, [\"proto\"] = 6 }} }} }}")).unwrap().skipped.is_empty(), "an empty realm is allowed");

        let text = format!
        (
            "SterlingQuestSyncOut = {{ [\"format\"] = 1, [\"characters\"] = {{ [\"{ME}\"] = {{ [\"name\"] = \"Kaelith Emberfall\", [\"realm\"] = \"Doom.howl: Forever\", [\"saved\"] = 1794770000, [\"proto\"] = 6 }}, [\"{BRAN}\"] = {{ [\"name\"] = \"Bran\", [\"realm\"] = \"R\", [\"saved\"] = 5, [\"proto\"] = 6 }} }} }}"
        );

        let outbox = parse_outbox(&text).unwrap();

        assert_eq!(outbox.characters.len(), 1);
        assert_eq!(outbox.characters[0].realm, "Doomhowl Forever");
        assert_eq!(outbox.skipped, vec![format!("{BRAN}: bad save time")]);
    }

    #[test]
    fn reads_whether_each_partner_has_the_app()
    {
        let row = |g: &str, app: &str| format!("[\"{g}\"] = {{ [\"packet\"] = \"AB\", [\"change\"] = 1{app} }},");
        let text = format!
        (
            "SterlingQuestSyncOut = {{ [\"format\"] = 1, [\"characters\"] = {{ [\"{ME}\"] = {{ [\"name\"] = \"Kaelith Emberfall\", [\"realm\"] = \"R\", [\"saved\"] = 1794770000, [\"proto\"] = 6, [\"partners\"] = {{ {} {} {} {} }} }} }} }}",
            row("Player-6800-0000000A", ", [\"app\"] = \"0.1.9\""),
            row("Player-6800-0000000B", ", [\"app\"] = false"),
            row("Player-6800-0000000C", ""),
            row("Player-6800-0000000D", ", [\"app\"] = \"1.0; os.exit()\""),
        );

        let outbox = parse_outbox(&text).unwrap();
        let app = |g: &str| outbox.characters[0].partners.iter().find(|p| p.guid == g).unwrap().app.clone();

        assert_eq!(app("Player-6800-0000000A"), PartnerApp::Has("0.1.9".into()));
        assert_eq!(app("Player-6800-0000000B"), PartnerApp::No);
        assert_eq!(app("Player-6800-0000000C"), PartnerApp::Unknown);
        assert_eq!(app("Player-6800-0000000D"), PartnerApp::Unknown, "bad version ignored");
        assert!(outbox.skipped.iter().any(|s| s.contains("bad app version")));
    }

    #[test]
    fn reads_level_class_and_race_for_the_window()
    {
        let text = format!
        (
            "SterlingQuestSyncOut = {{ [\"format\"] = 1, [\"characters\"] = {{ [\"{ME}\"] = {{ [\"name\"] = \"Kaelith Emberfall\", [\"realm\"] = \"R\", [\"saved\"] = 1794770000, [\"proto\"] = 6, [\"level\"] = 41, [\"class\"] = \"DRUID\", [\"race\"] = \"Troll\", [\"sterling\"] = \"0.2.40\", [\"partners\"] = {{ [\"{BRAN}\"] = {{ [\"packet\"] = \"AB\", [\"change\"] = 1, [\"level\"] = 900, [\"class\"] = \"<b>MAGE\", [\"race\"] = \"Night Elf\", [\"nick\"] = \"Bran\", [\"seen\"] = 1794770100 }} }} }} }} }}"
        );

        let outbox = parse_outbox(&text).unwrap();
        let character = &outbox.characters[0];

        assert_eq!(character.look, Look { level: Some(41), class: Some("DRUID".into()), race: Some("Troll".into()), nick: None, seen: None });
        assert_eq!(character.sterling.as_deref(), Some("0.2.40"));

        let look = &character.partners[0].look;

        assert_eq!(look.level, None, "out of range");
        assert_eq!(look.class, None, "not a class token");
        assert_eq!(look.race.as_deref(), Some("Night Elf"));
        assert_eq!(look.nick.as_deref(), Some("Bran"));
        assert_eq!(look.seen, Some(1794770100));
    }

    #[test]
    fn reads_favourites_up_to_five()
    {
        let guids: Vec<String> = (0..7).map(|i| format!("\"Player-6800-0000000{i}\"")).collect();
        let text = format!
        (
            "SterlingQuestSyncOut = {{ [\"format\"] = 1, [\"characters\"] = {{ [\"{ME}\"] = {{ [\"name\"] = \"Kaelith Emberfall\", [\"realm\"] = \"R\", [\"saved\"] = 1794770000, [\"proto\"] = 6, [\"favs\"] = {{ \"bad\", \"{ME}\", \"Player-6800-00000000\", {} }}, [\"favs_change\"] = 1794770000 }} }} }}",
            guids.join(", ")
        );

        let favs = parse_outbox(&text).unwrap().characters[0].favs.clone().unwrap();

        assert_eq!(favs.list.len(), 5, "bad GUID, self and repeats dropped, capped at 5");
        assert_eq!(favs.list[0], "Player-6800-00000000");
        assert_eq!(favs.change, 1794770000);
    }

    #[test]
    fn favourites_without_a_change_time_are_quietly_ignored()
    {
        let text = format!
        (
            "SterlingQuestSyncOut = {{ [\"format\"] = 1, [\"characters\"] = {{ [\"{ME}\"] = {{ [\"name\"] = \"Kaelith Emberfall\", [\"realm\"] = \"R\", [\"saved\"] = 1794770000, [\"proto\"] = 6, [\"favs\"] = {{ }} }} }} }}"
        );

        let outbox = parse_outbox(&text).unwrap();

        assert!(outbox.characters[0].favs.is_none());
        assert!(outbox.skipped.is_empty(), "not an error: {:?}", outbox.skipped);
    }

    #[test]
    fn missing_outbox_table_is_empty_not_an_error()
    {
        assert!(parse_outbox("SomethingElse = {}").unwrap().characters.is_empty());
        assert!(parse_outbox("SterlingQuestSyncOut = { [\"format\"] = 2 }").is_err());
    }

    fn sample_inbox() -> Inbox
    {
        let mut inbox = Inbox::default();

        inbox.put_packet(ME, InPacket { seq: 10, from: BRAN.into(), kind: Kind::Partner, time: 1794770000, proto: 6, data: "AAAA".into() });
        inbox.set_pairs
        (
            ME,
            vec![InPair
            {
                partner: BRAN.into(),
                my_key: Some("ABCDEFGHJKMNPQRS".into()),
                partner_key: Some("TVWXYZ2345678923".into()),
                status: PairStatus::Active,
                change: 2,
            }],
        );

        inbox
    }

    #[test]
    fn inbox_renders_to_lua_our_own_parser_reads_back()
    {
        let lua = render_inbox(&sample_inbox(), 1794770123).unwrap();
        let vars = parse_saved_variables(&lua).unwrap();
        let root = vars["SterlingQuestSync_Inbox"].as_table().unwrap();

        assert_eq!(root.get("format").unwrap().as_i64(), Some(1));

        let me = root.get("chars").unwrap().as_table().unwrap().get(ME).unwrap().as_table().unwrap();
        let packet = &me.get("packets").unwrap().as_table().unwrap().list[0];

        assert_eq!(packet.as_table().unwrap().get("data").unwrap().as_str(), Some("AAAA"));
        assert!(!lua.contains("Kaelith Emberfall"), "no names in the inbox");
    }

    #[test]
    fn inbox_refuses_anything_off_the_allowlist()
    {
        for data in ["x\"]] os.exit() --", "abc\ndef", "a b", "a'b", "a\\b", ""]
        {
            let mut inbox = Inbox::default();

            inbox.chars.entry(ME.into()).or_default().packets.push(InPacket
            {
                seq: 1,
                from: BRAN.into(),
                kind: Kind::Partner,
                time: 1794770000,
                proto: 6,
                data: data.into(),
            });

            assert!(render_inbox(&inbox, 1794770000).is_err(), "{data:?}");
        }

        let mut inbox = Inbox::default();

        inbox.chars.entry("Player-1-0\"]] x".into()).or_default();
        assert!(render_inbox(&inbox, 1794770000).is_err());

        let mut inbox = sample_inbox();

        inbox.chars.get_mut(ME).unwrap().pairs[0].my_key = Some("abc\"..os.exit()".into());
        assert!(render_inbox(&inbox, 1794770000).is_err());

        let mut inbox = sample_inbox();

        inbox.chars.get_mut(ME).unwrap().packets[0].from = "Player-6800-0A1B2C3D\"".into();
        assert!(render_inbox(&inbox, 1794770000).is_err());
    }

    #[test]
    fn newer_packets_replace_older_and_never_go_backwards()
    {
        let mut inbox = sample_inbox();
        let older = InPacket { seq: 5, from: BRAN.into(), kind: Kind::Partner, time: 1794770000, proto: 6, data: "OLD".into() };

        assert!(!inbox.put_packet(ME, older));

        let newer = InPacket { seq: 20, from: BRAN.into(), kind: Kind::Partner, time: 1794770100, proto: 6, data: "NEW".into() };

        assert!(inbox.put_packet(ME, newer));
        assert_eq!(inbox.chars[ME].packets.len(), 1);
        assert_eq!(inbox.chars[ME].packets[0].data, "NEW");
    }

    #[test]
    fn app_versions_go_in_and_bad_ones_are_refused()
    {
        let mut inbox = sample_inbox();

        inbox.app = Some(AppInfo { version: "0.1.2".into(), latest: Some("0.1.3".into()), sterling_latest: None });

        let lua = render_inbox(&inbox, 1794770123).unwrap();
        let vars = parse_saved_variables(&lua).unwrap();
        let app = vars["SterlingQuestSync_Inbox"].as_table().unwrap().get("app").unwrap().as_table().unwrap();

        assert_eq!(app.get("version").unwrap().as_str(), Some("0.1.2"));
        assert_eq!(app.get("latest").unwrap().as_str(), Some("0.1.3"));

        // an internal build's 4th part is fine; a 5th isn't
        assert!(valid_version("0.1.28.01") && valid_version("0.3.17.100"));

        for bad in ["0.1.3\" os.exit() --", "v0.1", "", "1.2.3.4.5", "1.2.3.4567"]
        {
            inbox.app = Some(AppInfo { version: "0.1.2".into(), latest: Some(bad.into()), sterling_latest: None });
            assert!(render_inbox(&inbox, 1794770123).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn empty_inbox_is_valid_lua()
    {
        let lua = empty_inbox_lua();

        assert!(parse_saved_variables(&lua).unwrap().contains_key("SterlingQuestSync_Inbox"));
    }
}
