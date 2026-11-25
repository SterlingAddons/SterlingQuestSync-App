//! One sync pass: outbox up, inbox down. Per-PC state lives in the app's data folder.
//!
//! Order matters for safety: everything is worked out on a copy of the state, Inbox.lua is written
//! (after a backup of Sterling's SavedVariables), and only then is the new state saved. A crash
//! part way leaves the old state, so the next pass simply fetches the same packets again.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::api::{Api, Character, Pairing, Upload};
use crate::companion;
use crate::formats::
{
    valid_guid, AppInfo, valid_packet, Favs, InOffer, PartnerApp, MAX_FAVS, InPacket, InPair, Inbox, Kind, Outbox, PairStatus, RE_PAIR_CODE, RE_PAIR_KEY,
};

use crate::safety::{Root, SafeFs};
use crate::{Error, Result};

pub const STATE_FORMAT: u32 = 1;
const MAX_INBOX_PAGES: usize = 50;
/// Auto-pairing: a code is replaced once it has less than this left (codes last 24 hours).
const OFFER_REFRESH: i64 = 6 * 3600;
/// Auto-pairing: open codes per character, so "Pair a friend" always has room left.
const MAX_OFFERS: usize = 8;

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Sent
{
    pub game_time: i64,
    pub hash: String,
    pub seq: i64,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct State
{
    pub format: u32,
    /// Highest server number already in the inbox.
    pub since: i64,
    pub inbox: Inbox,
    /// Characters as listed on the server, by GUID.
    pub listed: BTreeMap<String, Character>,
    /// Last upload per slot, keyed "from>to".
    pub sent: BTreeMap<String, Sent>,
    pub last_sync: Option<i64>,
    /// The server's pairings as of the last sync, without keys, for the window.
    #[serde(default)]
    pub pairs: Vec<PairView>,
    /// Auto-pairing: codes this PC offered, by my GUID, then partner GUID.
    #[serde(default)]
    pub offers: BTreeMap<String, BTreeMap<String, Offer>>,
    /// Auto-pairing: codes from partners already handled (a short hash each), so they're tried once.
    #[serde(default)]
    pub tried: BTreeSet<String>,
    /// Favourite partners per character (shared with Sterling; newest change wins).
    #[serde(default)]
    pub favs: BTreeMap<String, Favs>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Offer
{
    pub id: String,
    pub code: String,
    pub expires: i64,
}

/// A pairing as the window shows it. Never carries keys.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct PairView
{
    pub id: String,
    pub my_guid: String,
    pub partner_guid: String,
    pub partner_name: String,
    pub partner_realm: String,
    pub active: bool,
    pub ended_reason: Option<String>,
    pub created_at: i64,
    pub ended_at: Option<i64>,
}

impl From<&Pairing> for PairView
{
    fn from(source: &Pairing) -> Self
    {
        PairView
        {
            id: source.id.clone(),
            my_guid: source.my_guid.clone(),
            partner_guid: source.partner_guid.clone(),
            partner_name: source.partner_name.clone(),
            partner_realm: source.partner_realm.clone(),
            active: source.status == "active" && source.ended_at.is_none(),
            ended_reason: source.ended_reason.clone(),
            created_at: source.created_at,
            ended_at: source.ended_at,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct Report
{
    pub uploaded: usize,
    pub unchanged: usize,
    /// Slots where the server already had a newer save (from, to).
    pub stale: Vec<(String, String)>,
    /// Partners in the game with no live pairing yet (from, to).
    pub not_paired: Vec<(String, String)>,
    pub received: usize,
    /// (from, to) for each copy that arrived.
    pub received_from: Vec<(String, String)>,
    /// (from, to) for each copy that went up.
    pub sent_to: Vec<(String, String)>,
    pub ended: Vec<String>,
    pub inbox_written: bool,
    pub skipped: Vec<String>,
    /// Auto-pairing: new codes put in the inbox for Sterling to hand over.
    pub auto_offered: usize,
    /// Auto-pairing: (me, partner, partner name) paired this pass.
    pub auto_paired: Vec<(String, String, String)>,
    /// Auto-pairing: things the person may need to do by hand, in words.
    pub auto_notes: Vec<String>,
}

/// The key for one character's slot for one partner.
fn slot_key(from: &str, to: &str) -> String
{
    format!("{from}>{to}")
}

/// A short fingerprint of a packet, to tell if it changed.
fn hash(packet: &str) -> String
{
    let sha = Sha256::digest(packet.as_bytes());

    sha.iter().take(12).map(|b| format!("{b:02x}")).collect()
}

/// Where the saved sync state lives.
fn state_path(fs_: &SafeFs) -> std::path::PathBuf
{
    fs_.root(Root::AppData).join("state.json")
}

/// The saved sync state, or a fresh one if it's missing or from another version.
pub fn load_state(fs_: &SafeFs) -> State
{
    fs::read_to_string(state_path(fs_))
        .ok()
        .and_then(|t| serde_json::from_str::<State>(&t).ok())
        .filter(|s| s.format == STATE_FORMAT)
        .unwrap_or(State { format: STATE_FORMAT, ..Default::default() })
}

/// Writes the sync state.
pub fn save_state(fs_: &SafeFs, state: &State) -> Result<()>
{
    let text = serde_json::to_string_pretty(state).map_err(|e| Error::Other(e.to_string()))?;

    fs_.write_atomic(&state_path(fs_), text.as_bytes())
}

/// Characters with a save newer than what this PC last uploaded: the "ready to sync" line.
pub fn pending(state: &State, outboxes: &[Outbox]) -> Vec<(String, i64)>
{
    let mut out = Vec::new();

    for outbox in outboxes
    {
        for character in &outbox.characters
        {
            let mut slots: Vec<(&str, &str)> = Vec::new();

            if let Some(packet) = &character.self_packet
            {
                slots.push((&character.guid, packet));
            }

            // Partners count only when the last sync saw a live pairing with them.
            let live =
                |g: &str| state.inbox.chars.get(&character.guid).is_some_and(|ch| ch.pairs.iter().any(|p| p.partner == g && p.status == PairStatus::Active));

            for partner in &character.partners
            {
                if let Some(packet) = &partner.packet
                {
                    if live(&partner.guid)
                    {
                        slots.push((&partner.guid, packet));
                    }
                }
            }

            let ready = slots.iter().any(|(to, packet)| match state.sent.get(&slot_key(&character.guid, to))
            {
                Some(s) => s.game_time < character.saved && s.hash != hash(packet),
                None => true,
            });

            if ready
            {
                out.push((character.guid.clone(), character.saved));
            }
        }
    }

    out
}

/// This character's live pairing with a partner, if there is one.
fn active_pair<'a>(pairings: &'a [Pairing], me: &str, partner: &str) -> Option<&'a Pairing>
{
    pairings.iter().find(|p| p.status == "active" && p.ended_at.is_none() && p.my_guid == me && p.partner_guid == partner)
}

/// A pairing key only if it has the right shape.
fn pair_key(key: &Option<String>) -> Option<String>
{
    key.as_ref().filter(|v| RE_PAIR_KEY.is_match(v)).cloned()
}

/// Turns the server's pairings into the inbox's pairs per character.
fn pairs_for(pairings: &[Pairing], me: &str) -> Vec<InPair>
{
    // One entry per partner: an active pairing beats an ended one; newer beats older.
    let mut best: BTreeMap<&str, &Pairing> = BTreeMap::new();

    for pair_row in pairings.iter().filter(|pair_row| pair_row.my_guid == me && valid_guid(&pair_row.partner_guid))
    {
        let rank = |x: &Pairing| (x.status == "active", x.ended_at.unwrap_or(x.created_at).max(x.created_at));

        match best.get(pair_row.partner_guid.as_str())
        {
            Some(cur) if rank(cur) >= rank(pair_row) => {}
            _ =>
            {
                best.insert(&pair_row.partner_guid, pair_row);
            }
        }
    }

    best.values()
        .map(|pairing|
        {
            let active = pairing.status == "active" && pairing.ended_at.is_none();

            InPair
            {
                partner: pairing.partner_guid.clone(),
                my_key: if active { pair_key(&pairing.my_key) } else { None },
                partner_key: if active { pair_key(&pairing.partner_key) } else { None },
                status: if active { PairStatus::Active } else { PairStatus::Ended },
                change: if active { pairing.created_at } else { pairing.ended_at.unwrap_or(pairing.created_at) },
            }
        })
        .collect()
}

/// The key that remembers a pair code was already tried.
fn tried_key(me: &str, from: &str, code: &str) -> String
{
    hash(&format!("{me}|{from}|{code}"))
}

/// Network trouble or a busy server: stop auto-pairing for this pass and try again next time.
fn try_later(error: &Error) -> bool
{
    matches!(error, Error::Offline)
        || matches!(error, Error::Api { code, .. } if code == "rate_limited" || code == "server_error")
        || matches!(error, Error::Server(_))
}

/// Redeems codes from `codes[sender]` in the outbox. Returns true when a pairing was made.
fn redeem_codes(api: &dyn Api, state: &mut State, rep: &mut Report, outboxes: &[Outbox], pairings: &[Pairing]) -> bool
{
    let mut made = false;

    for character_out in outboxes.iter().flat_map(|o| &o.characters)
    {
        for (from, code) in &character_out.codes
        {
            // Only from someone who is a Sterling partner of this character right now.
            let Some(partner) = character_out.partners.iter().find(|p| &p.guid == from && !p.removed) else { continue };

            if active_pair(pairings, &character_out.guid, from).is_some()
            {
                continue;
            }

            let key = tried_key(&character_out.guid, from, code);

            if state.tried.contains(&key)
            {
                continue;
            }

            let label = partner.name.clone().unwrap_or_else(|| "a Sterling partner".into());
            let peek = match api.peek_code(&character_out.guid, code)
            {
                Ok(p) => p,
                Err(e) if try_later(&e) => return made,
                Err(_) =>
                {
                    // not_found, own_code and the like: the code is spent or wrong; never retry it.
                    state.tried.insert(key);
                    continue;
                }
            };

            // The code must belong to the very character Sterling says sent it.
            if &peek.partner.guid != from
            {
                state.tried.insert(key);
                rep.skipped.push("pairing code that didn't match its sender".into());
                continue;
            }

            if peek.different_install
            {
                state.tried.insert(key);
                rep.auto_notes.push(format!("{} is already synced from another install. Pair by hand in Partners if you want to switch.", peek.partner.name));
                continue;
            }

            match api.redeem_code(&character_out.guid, code, false)
            {
                Ok(_) =>
                {
                    state.tried.insert(key);
                    rep.auto_paired.push((character_out.guid.clone(), from.clone(), peek.partner.name.clone()));
                    made = true;
                }

                Err(e) if try_later(&e) => return made,
                Err(Error::Api { code: why, .. }) =>
                {
                    state.tried.insert(key);

                    if why == "partner_limit"
                    {
                        rep.auto_notes.push(format!("Couldn't pair with {label}: one of you already has the most partners allowed."));
                    }
                }

                Err(_) =>
                {
                    state.tried.insert(key);
                }
            }
        }
    }

    // Keep the list from growing forever: only codes still in some outbox matter.
    let live: BTreeSet<String> = outboxes.iter().flat_map(|o| &o.characters).flat_map(|c| c.codes.iter().map(move |(f, k)| tried_key(&c.guid, f, k))).collect();

    state.tried.retain(|k| live.contains(k));
    made
}

/// Keeps one fresh code per unpaired Sterling partner in the inbox, and cancels codes no longer needed.
fn offer_codes(api: &dyn Api, state: &mut State, rep: &mut Report, outboxes: &[Outbox], pairings: &[Pairing], now: i64)
{
    // Who should get a code: Sterling partners of characters on this PC with no live pairing, whose
    // Sterling told ours in game that they have the app (no point using up a code slot otherwise).
    let mut want: BTreeMap<String, Vec<String>> = BTreeMap::new();

    for character_out in outboxes.iter().flat_map(|o| &o.characters)
    {
        let list = want.entry(character_out.guid.clone()).or_default();

        for partner_out in character_out.partners.iter().filter(|partner_out|
        {
            !partner_out.removed && valid_guid(&partner_out.guid) && partner_out.guid != character_out.guid && matches!(partner_out.app, PartnerApp::Has(_))
        })
        {
            if active_pair(pairings, &character_out.guid, &partner_out.guid).is_none() && !list.contains(&partner_out.guid)
            {
                list.push(partner_out.guid.clone());
            }
        }

        list.truncate(MAX_OFFERS);
    }

    // Cancel what isn't wanted any more (paired, removed, or the character left this PC).
    let mut stop: Vec<Offer> = Vec::new();

    for (me, offers) in state.offers.iter_mut()
    {
        let wanted = want.get(me);

        offers.retain(|partner, offer|
        {
            let keep = wanted.is_some_and(|l| l.contains(partner));

            if !keep
            {
                stop.push(offer.clone());
            }

            keep
        });
    }

    state.offers.retain(|_, o| !o.is_empty());

    for stale in stop
    {
        // Already used or expired is fine; anything else the server clears within a day.
        let _ = api.cancel_code(&stale.id);
    }

    // Make or refresh codes.
    let mut busy = false;

    for (me, partners) in &want
    {
        for partner in partners
        {
            if busy
            {
                break;
            }

            let cur = state.offers.get(me).and_then(|m| m.get(partner)).cloned();

            if cur.as_ref().is_some_and(|o| o.expires - now > OFFER_REFRESH)
            {
                continue;
            }

            match api.pair_code(me)
            {
                Ok(code) =>
                {
                    let id = code.id.clone().unwrap_or_default();

                    if !RE_PAIR_CODE.is_match(&code.code) || id.is_empty() || code.expires_at <= now
                    {
                        rep.skipped.push("pairing code from the server that failed checks".into());

                        if !id.is_empty()
                        {
                            let _ = api.cancel_code(&id);
                        }

                        busy = true;
                        continue;
                    }

                    if let Some(old) = cur
                    {
                        let _ = api.cancel_code(&old.id);
                    }

                    state.offers.entry(me.clone()).or_default().insert(partner.clone(), Offer { id, code: code.code, expires: code.expires_at });
                    rep.auto_offered += 1;
                }

                Err(Error::Api { code, .. }) if code == "partner_limit" =>
                {
                    // No room for another open code on this character; the rest wait.
                    break;
                }

                Err(_) => busy = true,
            }
        }
    }

    // Put the offers in the inbox for every character on this PC.
    let mut chars: BTreeSet<String> = want.keys().cloned().collect();

    chars.extend(state.inbox.chars.iter().filter(|(_, c)| !c.offers.is_empty()).map(|(g, _)| g.clone()));

    for me in chars
    {
        let offers: Vec<InOffer> = state
            .offers
            .get(&me)
            .map(|m| m.iter().filter(|(_, o)| o.expires > now).map(|(p, o)| InOffer { partner: p.clone(), code: o.code.clone(), expires: o.expires }).collect())
            .unwrap_or_default();

        state.inbox.set_offers(&me, offers);
    }
}

/// Favourites changed in game more recently than in the app replace the app's list; the app's list
/// goes into the inbox so Sterling can take it when it's the newer one.
fn take_favs(state: &mut State, outboxes: &[Outbox])
{
    for character in outboxes.iter().flat_map(|o| &o.characters)
    {
        if let Some(favs) = &character.favs
        {
            if state.favs.get(&character.guid).is_none_or(|mine| favs.change > mine.change)
            {
                state.favs.insert(character.guid.clone(), favs.clone());
            }
        }
    }

    let targets: Vec<String> = state.favs.keys().cloned().collect();

    for guid in targets
    {
        let favs = state.favs.get(&guid).cloned();

        state.inbox.set_favs(&guid, favs);
    }
}

/// Star or unstar a partner in the app. Writes the inbox right away (no network needed), so the game
/// picks it up at the next /reload. Returns false when 5 are already starred.
pub fn set_favorite(fs_: &SafeFs, state: &mut State, me: &str, partner: &str, on: bool, now: i64) -> Result<bool>
{
    if !valid_guid(me) || !valid_guid(partner) || me == partner
    {
        return Err(Error::Refused("bad character".into()));
    }

    let mut next = state.clone();
    let cur = next.favs.get(me).cloned().unwrap_or_default();
    let mut list = cur.list.clone();

    if on
    {
        if list.iter().any(|g| g == partner)
        {
            return Ok(true);
        }

        if list.len() >= MAX_FAVS
        {
            return Ok(false);
        }

        list.push(partner.to_string());
    }

    else
    {
        list.retain(|g| g != partner);
    }

    // Always newer than anything seen, even if this PC's clock is a little behind the game's.
    let favs = Favs::clean(list, now.max(cur.change + 1));

    next.favs.insert(me.to_string(), favs.clone());

    if next.inbox.set_favs(me, Some(favs))
    {
        companion::backup_saved_variables(fs_, "favourites")?;
        companion::write_inbox(fs_, &next.inbox, now)?;
    }

    save_state(fs_, &next)?;
    *state = next;
    Ok(true)
}

/// Shown for a character whose game reported no realm and who isn't on the server yet.
pub const UNKNOWN_REALM: &str = "Unknown realm";

/// One full pass. `now` is the wall clock, used only for `written` and `last_sync`.
/// `app` carries the versions the add-on shows; None leaves what the inbox has.
pub fn sync(api: &dyn Api, fs_: &SafeFs, state: &mut State, outboxes: &[Outbox], app: Option<AppInfo>, now: i64) -> Result<Report>
{
    let mut next = state.clone();

    next.format = STATE_FORMAT;

    let mut rep = Report::default();

    for each_outbox in outboxes
    {
        rep.skipped.extend(each_outbox.skipped.iter().cloned());
    }

    // 1. Make sure every character in the outbox is listed, with its current name.
    for character_out in outboxes.iter().flat_map(|o| &o.characters)
    {
        // No realm from the game: keep the one the server already has, or a stand-in the server accepts.
        let realm = match character_out.realm.as_str()
        {
            "" => next
                .listed
                .get(&character_out.guid)
                .map(|known| known.realm.clone())
                .filter(|known| !known.is_empty())
                .unwrap_or_else(|| UNKNOWN_REALM.to_string()),
            given => given.to_string(),
        };

        let listed = Character { guid: character_out.guid.clone(), name: character_out.name.clone(), realm };

        if next.listed.get(&character_out.guid) != Some(&listed)
        {
            api.put_character(&listed)?;
            next.listed.insert(character_out.guid.clone(), listed);
        }
    }

    // 2. Partners removed in game end the pairing, but only one made before the removal.
    let mut pairings = api.pairings()?;
    let mut ended_any = false;

    for character_out in outboxes.iter().flat_map(|o| &o.characters)
    {
        for partner_out in character_out.partners.iter().filter(|partner_out| partner_out.removed)
        {
            if let Some(pair) = active_pair(&pairings, &character_out.guid, &partner_out.guid)
            {
                if partner_out.change > pair.created_at
                {
                    api.end_pairing(&pair.id)?;
                    rep.ended.push(pair.id.clone());
                    ended_any = true;
                }
            }
        }
    }

    if ended_any
    {
        pairings = api.pairings()?;
    }

    // 2b. Auto-pairing: redeem codes Sterling got from partners in game. Runs before uploads so a
    // new pairing gets its copy right away. Never fails the sync; network trouble just waits.
    if redeem_codes(api, &mut next, &mut rep, outboxes, &pairings)
    {
        pairings = api.pairings()?;
    }

    // 3. Uploads: the self copy, then one per live partner.
    for character_out in outboxes.iter().flat_map(|o| &o.characters)
    {
        let mut slots: Vec<(&str, &str)> = Vec::new();

        if let Some(self_packet) = &character_out.self_packet
        {
            slots.push((&character_out.guid, self_packet));
        }

        for partner_out in character_out.partners.iter().filter(|partner_out| !partner_out.removed)
        {
            let Some(packet_text) = &partner_out.packet else { continue };

            if active_pair(&pairings, &character_out.guid, &partner_out.guid).is_none()
            {
                rep.not_paired.push((character_out.guid.clone(), partner_out.guid.clone()));
                continue;
            }

            slots.push((&partner_out.guid, packet_text));
        }

        for (to, packet_text) in slots
        {
            let key = slot_key(&character_out.guid, to);
            let hashed = hash(packet_text);

            if let Some(last_sent) = next.sent.get(&key)
            {
                if last_sent.game_time >= character_out.saved && (last_sent.hash == hashed || last_sent.game_time > character_out.saved)
                {
                    rep.unchanged += 1;
                    continue;
                }
            }

            match api.put_slot(&character_out.guid, to, packet_text, character_out.saved, character_out.proto)
            {
                Ok(Upload::Saved(seq)) =>
                {
                    rep.uploaded += 1;
                    rep.sent_to.push((character_out.guid.clone(), to.to_string()));
                    next.sent.insert(key, Sent { game_time: character_out.saved, hash: hashed, seq });
                }

                Ok(Upload::Unchanged(seq)) =>
                {
                    rep.unchanged += 1;
                    next.sent.insert(key, Sent { game_time: character_out.saved, hash: hashed, seq });
                }

                Ok(Upload::Stale { seq, game_time }) =>
                {
                    rep.stale.push((character_out.guid.clone(), to.to_string()));
                    // Remember the newer save so this one isn't sent again.
                    next.sent.insert(key, Sent { game_time, hash: String::new(), seq });
                }

                Err(Error::Api { code, .. }) if code == "not_paired" =>
                {
                    rep.not_paired.push((character_out.guid.clone(), to.to_string()));
                }

                Err(e) => return Err(e),
            }
        }
    }

    // 4. Downloads, page by page.
    let before = next.inbox.clone();

    offer_codes(api, &mut next, &mut rep, outboxes, &pairings, now);
    take_favs(&mut next, outboxes);

    if let Some(app_info) = app
    {
        next.inbox.app = Some(app_info);
    }

    let mine: BTreeSet<String> = next.listed.keys().cloned().collect();

    for _ in 0..MAX_INBOX_PAGES
    {
        let page = api.inbox(next.since)?;

        for page_packet in &page.packets
        {
            let kind = match page_packet.kind.as_str()
            {
                "partner" => Kind::Partner,
                "self" => Kind::SelfCopy,
                _ =>
                {
                    rep.skipped.push("packet of an unknown kind".into());
                    continue;
                }
            };

            let fine = valid_guid(&page_packet.from_guid)
                && valid_guid(&page_packet.to_guid)
                && valid_packet(&page_packet.packet)
                && (1_000_000_000..4_000_000_000).contains(&page_packet.game_time)
                && (1..=10_000).contains(&page_packet.proto)
                && page_packet.seq > 0;

            if !fine || (kind == Kind::SelfCopy && page_packet.from_guid != page_packet.to_guid)
            {
                rep.skipped.push("packet that failed checks".into());
                continue;
            }

            // Our own upload coming back: nothing new for this PC.
            if kind == Kind::SelfCopy && next.sent.get(&slot_key(&page_packet.from_guid, &page_packet.to_guid)).map(|s| s.seq) == Some(page_packet.seq)
            {
                continue;
            }

            let in_packet = InPacket
            {
                seq: page_packet.seq,
                from: page_packet.from_guid.clone(),
                kind,
                time: page_packet.game_time,
                proto: page_packet.proto,
                data: page_packet.packet.clone(),
            };

            if next.inbox.put_packet(&page_packet.to_guid, in_packet)
            {
                rep.received += 1;
                rep.received_from.push((page_packet.from_guid.clone(), page_packet.to_guid.clone()));
            }
        }

        next.since = next.since.max(page.next);

        if !page.more
        {
            break;
        }
    }

    // 5. Pairs for every character that has any, and drop copies from ended pairings.
    let mut targets: BTreeSet<String> = next.inbox.chars.keys().cloned().collect();

    targets.extend(pairings.iter().map(|p| p.my_guid.clone()).filter(|g| valid_guid(g)));

    for target in &targets
    {
        let pairs = pairs_for(&pairings, target);
        let live: BTreeSet<&str> = pairs.iter().filter(|p| p.status == PairStatus::Active).map(|p| p.partner.as_str()).collect();

        next.inbox.set_pairs(target, pairs.clone());

        if let Some(inbox_char) = next.inbox.chars.get_mut(target)
        {
            inbox_char.packets.retain(|p| p.kind == Kind::SelfCopy || live.contains(p.from.as_str()));
        }
    }

    next.inbox.chars.retain(|g, c| !c.packets.is_empty() || !c.pairs.is_empty() || !c.offers.is_empty() || mine.contains(g));
    next.pairs = pairings.iter().map(PairView::from).collect();

    // 6. Write the inbox (backup first), then commit the state.
    if next.inbox != before || !companion::is_installed(fs_)
    {
        if next.inbox != before
        {
            companion::backup_saved_variables(fs_, "sync")?;
        }

        rep.inbox_written = companion::write_inbox(fs_, &next.inbox, now)?;
    }

    next.last_sync = Some(now);
    save_state(fs_, &next)?;
    *state = next;
    Ok(rep)
}

#[cfg(test)]
mod tests
{
    use super::*;
    use crate::api::{Code, InboxPage, Peek, Redeemed, ServerPacket};
    use crate::formats::{parse_outbox, CharacterOut, PartnerOut};
    use std::cell::RefCell;

    const ME: &str = "Player-6800-0000A11C";
    const BRAN: &str = "Player-6800-0A1B2C3D";
    const ALT: &str = "Player-6800-0000ABCD";

    #[derive(Default)]
    struct Fake
    {
        pairings: RefCell<Vec<Pairing>>,
        slots: RefCell<BTreeMap<String, (i64, String, i64)>>,
        incoming: RefCell<Vec<ServerPacket>>,
        puts: RefCell<usize>,
        chars: RefCell<usize>,
        ended: RefCell<Vec<String>>,
        seq: RefCell<i64>,
        /// Open codes: id -> (code, owner character GUID, owner name).
        codes: RefCell<BTreeMap<String, (String, String, String)>>,
        cancelled: RefCell<Vec<String>>,
        redeemed: RefCell<Vec<String>>,
        next_code: RefCell<u32>,
        offline: RefCell<bool>,
    }

    impl Api for Fake
    {
        fn put_character(&self, _c: &Character) -> Result<()>
        {
            *self.chars.borrow_mut() += 1;
            Ok(())
        }

        fn pairings(&self) -> Result<Vec<Pairing>>
        {
            Ok(self.pairings.borrow().clone())
        }

        fn end_pairing(&self, id: &str) -> Result<()>
        {
            self.ended.borrow_mut().push(id.into());

            for pairing in self.pairings.borrow_mut().iter_mut().filter(|p| p.id == id)
            {
                pairing.ended_at = Some(1_794_779_000);
                pairing.status = "ended".into();
                pairing.my_key = None;
                pairing.partner_key = None;
            }

            Ok(())
        }

        fn put_slot(&self, from: &str, to: &str, packet: &str, game_seconds: i64, _proto: i64) -> Result<Upload>
        {
            *self.puts.borrow_mut() += 1;

            let key = slot_key(from, to);
            let mut slots = self.slots.borrow_mut();

            if let Some((cur, _, seq)) = slots.get(&key)
            {
                if *cur > game_seconds
                {
                    return Ok(Upload::Stale { seq: *seq, game_time: *cur });
                }
            }

            *self.seq.borrow_mut() += 1;

            let seq = *self.seq.borrow();

            slots.insert(key, (game_seconds, packet.into(), seq));
            Ok(Upload::Saved(seq))
        }

        fn inbox(&self, since: i64) -> Result<InboxPage>
        {
            let all: Vec<_> = self.incoming.borrow().iter().filter(|p| p.seq > since).cloned().collect();
            let more = all.len() > 2;
            let packets: Vec<_> = all.into_iter().take(2).collect();
            let next = packets.last().map(|p| p.seq).unwrap_or(since);

            Ok(InboxPage { packets, next, more })
        }

        fn pair_code(&self, guid: &str) -> Result<Code>
        {
            if *self.offline.borrow()
            {
                return Err(Error::Offline);
            }

            *self.next_code.borrow_mut() += 1;

            let next = *self.next_code.borrow();
            let code = format!("AAAA-BBBB-{:0>4}", next).replace('0', "C").replace('1', "D");
            let id = format!("c{next}");

            self.codes.borrow_mut().insert(id.clone(), (code.clone(), guid.into(), "Kaelith Emberfall".into()));
            Ok(Code { code, expires_at: 1_794_770_000 + 86_400, id: Some(id), guid: Some(guid.into()) })
        }

        fn cancel_code(&self, id: &str) -> Result<()>
        {
            self.cancelled.borrow_mut().push(id.into());
            self.codes.borrow_mut().remove(id);
            Ok(())
        }

        fn peek_code(&self, _guid: &str, code: &str) -> Result<Peek>
        {
            if *self.offline.borrow()
            {
                return Err(Error::Offline);
            }

            let codes = self.codes.borrow();
            let Some((_, owner, name)) = codes.values().find(|(c, _, _)| c == code) else
            {
                return Err(Error::Api { status: 404, code: "not_found".into(), body: String::new() });
            };

            Ok(Peek { partner: Character { guid: owner.clone(), name: name.clone(), realm: "R".into() }, already_paired: false, different_install: false })
        }

        fn redeem_code(&self, guid: &str, code: &str, _replace: bool) -> Result<Redeemed>
        {
            let found = self.codes.borrow().iter().find(|(_, (c, _, _))| c == code).map(|(id, (_, o, _))| (id.clone(), o.clone()));
            let Some((id, owner)) = found else
            {
                return Err(Error::Api { status: 404, code: "not_found".into(), body: String::new() });
            };

            self.codes.borrow_mut().remove(&id);
            self.redeemed.borrow_mut().push(code.into());

            let mut made = pairing("auto", guid, &owner, 1_794_770_050);

            made.partner_name = "Bran".into();
            self.pairings.borrow_mut().push(made);
            Ok(Redeemed { pairing_id: "auto".into(), already_paired: false, replaced: 0 })
        }
    }

    fn pairing(id: &str, me: &str, partner: &str, created: i64) -> Pairing
    {
        Pairing
        {
            id: id.into(),
            created_at: created,
            ended_at: None,
            ended_reason: None,
            my_guid: me.into(),
            partner_guid: partner.into(),
            partner_name: "Bran".into(),
            partner_realm: "CAMWOW US2 Realm 2 PvP".into(),
            my_key: Some("ABCDEFGHJKMNPQRS".into()),
            partner_key: Some("TVWXYZ2345678923".into()),
            status: "active".into(),
        }
    }

    fn outbox(saved: i64, packet: &str, removed: Option<i64>) -> Outbox
    {
        let mut partners = vec![PartnerOut
        {
            guid: BRAN.into(),
            packet: Some(packet.into()),
            change: 1,
            removed: false,
            name: None,
            app: PartnerApp::Unknown,
            look: Default::default(),
        }];

        if let Some(character) = removed
        {
            partners[0] = PartnerOut
            {
                guid: BRAN.into(),
                packet: None,
                change: character,
                removed: true,
                name: None,
                app: PartnerApp::Unknown,
                look: Default::default(),
            };
        }

        partners.push(PartnerOut
        {
            guid: ALT.into(),
            packet: Some("ALTCOPY".into()),
            change: 1,
            removed: false,
            name: None,
            app: PartnerApp::Has("0.1.9".into()),
            look: Default::default(),
        });

        Outbox
        {
            characters: vec![CharacterOut
            {
                guid: ME.into(),
                name: "Kaelith Emberfall".into(),
                realm: "CAMWOW US2 Realm 2 PvP".into(),
                saved,
                proto: 6,
                self_packet: Some("SELF1".into()),
                partners,
                applied: Default::default(),
                codes: Default::default(),
                look: Default::default(),
                sterling: None,
                favs: None,
            }],

            skipped: vec![],
        }
    }

    fn incoming(seq: i64, from: &str, to: &str, kind: &str, data: &str) -> ServerPacket
    {
        ServerPacket { from_guid: from.into(), to_guid: to.into(), packet: data.into(), game_time: 1_794_770_000 + seq, proto: 6, seq, kind: kind.into() }
    }

    fn setup() -> (tempfile::TempDir, SafeFs)
    {
        let temp = tempfile::tempdir().unwrap();
        let game = temp.path().join("_forever_");

        fs::create_dir_all(game.join("Interface/AddOns/Questie")).unwrap();

        let saved_vars = game.join("WTF/Account/TESTACCOUNT/SavedVariables");

        fs::create_dir_all(&saved_vars).unwrap();
        fs::write(saved_vars.join("Sterling.lua"), "SterlingDB = {}").unwrap();

        let safe = SafeFs::new(&game, &temp.path().join("appdata")).unwrap();

        companion::install(&safe, &[("SterlingQuestSync.toc", crate::formats::companion_toc().as_bytes())]).unwrap();
        (temp, safe)
    }

    #[test]
    fn no_realm_from_the_game_keeps_the_known_one()
    {
        let (_t, fs_) = setup();
        let api = Fake::default();
        let mut state = load_state(&fs_);
        let mut blank = outbox(1_794_770_000, "COPY1", None);

        blank.characters[0].realm = String::new();
        sync(&api, &fs_, &mut state, std::slice::from_ref(&blank), None, 1_794_770_100).unwrap();

        assert_eq!(state.listed[ME].realm, UNKNOWN_REALM, "new character: a stand-in");

        state.listed.get_mut(ME).unwrap().realm = "CAMWOW US2 Realm 2 PvP".into();
        sync(&api, &fs_, &mut state, std::slice::from_ref(&blank), None, 1_794_770_200).unwrap();

        assert_eq!(state.listed[ME].realm, "CAMWOW US2 Realm 2 PvP", "known character keeps its realm");
        assert_eq!(*api.chars.borrow(), 1, "nothing changed, so nothing sent again");
    }

    #[test]
    fn uploads_once_then_skips_until_a_new_save()
    {
        let (_t, fs_) = setup();
        let api = Fake::default();

        api.pairings.borrow_mut().push(pairing("p1", ME, BRAN, 1_794_000_000));

        let mut state = load_state(&fs_);
        let report = sync(&api, &fs_, &mut state, &[outbox(1_794_770_000, "COPY1", None)], None, 1_794_770_100).unwrap();

        assert_eq!(report.uploaded, 2, "self and Bran");
        assert_eq!(report.not_paired, vec![(ME.to_string(), ALT.to_string())]);
        assert_eq!(*api.chars.borrow(), 1);

        let report = sync(&api, &fs_, &mut state, &[outbox(1_794_770_000, "COPY1", None)], None, 1_794_770_200).unwrap();

        assert_eq!(report.uploaded, 0);
        assert_eq!(*api.chars.borrow(), 1, "already listed");
        assert!(pending(&state, &[outbox(1_794_770_000, "COPY1", None)]).is_empty());
        assert_eq!(pending(&state, &[outbox(1_794_770_500, "COPY2", None)]).len(), 1);

        let report = sync(&api, &fs_, &mut state, &[outbox(1_794_770_500, "COPY2", None)], None, 1_794_770_600).unwrap();

        assert_eq!(report.uploaded, 2);
    }

    #[test]
    fn an_older_save_never_replaces_a_newer_one()
    {
        let (_t, fs_) = setup();
        let api = Fake::default();

        api.pairings.borrow_mut().push(pairing("p1", ME, BRAN, 1_794_000_000));
        api.slots.borrow_mut().insert(slot_key(ME, BRAN), (1_794_779_999, "NEWER".into(), 99));

        let mut state = load_state(&fs_);
        let report = sync(&api, &fs_, &mut state, &[outbox(1_794_770_000, "OLDER", None)], None, 1).unwrap();

        assert_eq!(report.stale, vec![(ME.to_string(), BRAN.to_string())]);
        assert_eq!(api.slots.borrow()[&slot_key(ME, BRAN)].1, "NEWER");

        let puts = *api.puts.borrow();

        sync(&api, &fs_, &mut state, &[outbox(1_794_770_000, "OLDER", None)], None, 2).unwrap();
        assert_eq!(*api.puts.borrow(), puts, "not retried");
    }

    #[test]
    fn inbox_receives_pages_and_writes_lua()
    {
        let (_t, fs_) = setup();
        let api = Fake::default();

        api.pairings.borrow_mut().push(pairing("p1", ME, BRAN, 1_794_000_000));
        api.incoming.borrow_mut().extend
        ([
            incoming(1, BRAN, ME, "partner", "OLD"),
            incoming(2, ME, ME, "self", "SELFFROMPC2"),
            incoming(3, BRAN, ME, "partner", "NEW"),
            incoming(4, BRAN, ME, "partner", "bad packet\"]] os.exit()"),
            incoming(5, "Player-6800-DEADBEEF", ME, "partner", "STRANGER"),
        ]);

        let mut sync_state = load_state(&fs_);
        let report = sync(&api, &fs_, &mut sync_state, &[outbox(1_794_770_000, "C", None)], None, 1_794_770_100).unwrap();

        assert!(report.inbox_written);
        assert_eq!(sync_state.since, 5);

        let me = &sync_state.inbox.chars[ME];
        let from_bran: Vec<_> = me.packets.iter().filter(|p| p.from == BRAN).collect();

        assert_eq!(from_bran.len(), 1);
        assert_eq!(from_bran[0].data, "NEW");
        assert!(me.packets.iter().any(|p| p.kind == Kind::SelfCopy && p.data == "SELFFROMPC2"));
        assert!(!me.packets.iter().any(|p| p.data == "STRANGER"), "no live pairing, dropped");
        assert_eq!(me.pairs[0].my_key.as_deref(), Some("ABCDEFGHJKMNPQRS"));

        let lua = fs::read_to_string(fs_.root(Root::Companion).join("Inbox.lua")).unwrap();

        assert!(lua.contains("\"NEW\"") && !lua.contains("os.exit"));
        assert!(!companion::list_backups(&fs_).is_empty(), "backup before the inbox changed");

        let saved = load_state(&fs_);

        assert_eq!(saved.since, 5, "state committed");
    }

    #[test]
    fn removing_a_partner_in_game_ends_only_the_older_pairing()
    {
        let (_t, fs_) = setup();
        let api = Fake::default();

        api.pairings.borrow_mut().push(pairing("p1", ME, BRAN, 1_794_000_000));

        let mut state = load_state(&fs_);
        // Removal from before this pairing was made: ignored (they paired again since).
        let report = sync(&api, &fs_, &mut state, &[outbox(1_794_770_000, "C", Some(1_793_000_000))], None, 1).unwrap();

        assert!(report.ended.is_empty());

        let report = sync(&api, &fs_, &mut state, &[outbox(1_794_770_000, "C", Some(1_794_500_000))], None, 2).unwrap();

        assert_eq!(report.ended, vec!["p1".to_string()]);

        let pair = &state.inbox.chars[ME].pairs[0];

        assert_eq!(pair.status, PairStatus::Ended);
        assert!(pair.my_key.is_none() && pair.partner_key.is_none(), "no keys once ended");
    }

    #[test]
    fn outbox_from_disk_flows_through()
    {
        let (_t, fs_) = setup();
        let text = format!
        (
            "SterlingQuestSyncOut = {{ [\"format\"] = 1, [\"characters\"] = {{ [\"{ME}\"] = {{ [\"name\"] = \"Kaelith Emberfall\", [\"realm\"] = \"CAMWOW US2 Realm 2 PvP\", [\"saved\"] = 1794770000, [\"proto\"] = 6, [\"partners\"] = {{ [\"{BRAN}\"] = {{ [\"packet\"] = \"ABC\", [\"change\"] = 1 }} }} }} }} }}"
        );

        let outbox = parse_outbox(&text).unwrap();
        let api = Fake::default();

        api.pairings.borrow_mut().push(pairing("p1", ME, BRAN, 1_794_000_000));

        let mut state = load_state(&fs_);
        let report = sync(&api, &fs_, &mut state, &[outbox], None, 1).unwrap();

        assert_eq!(report.uploaded, 1);
        assert_eq!(api.slots.borrow()[&slot_key(ME, BRAN)].1, "ABC");
    }

    #[test]
    fn offers_a_code_to_each_unpaired_partner_and_keeps_it()
    {
        let (_t, fs_) = setup();
        let api = Fake::default();

        api.pairings.borrow_mut().push(pairing("p1", ME, BRAN, 1_794_000_000));

        let mut state = load_state(&fs_);
        let now = 1_794_770_100;
        let report = sync(&api, &fs_, &mut state, &[outbox(1_794_770_000, "C", None)], None, now).unwrap();

        assert_eq!(report.auto_offered, 1, "only ALT is unpaired");

        let offers = &state.inbox.chars[ME].offers;

        assert_eq!(offers.len(), 1);
        assert_eq!(offers[0].partner, ALT);
        assert!(RE_PAIR_CODE.is_match(&offers[0].code));

        let lua = fs::read_to_string(fs_.root(Root::Companion).join("Inbox.lua")).unwrap();

        assert!(lua.contains("offers = {") && lua.contains(&offers[0].code));

        // Next pass keeps the same code.
        let report = sync(&api, &fs_, &mut state, &[outbox(1_794_770_000, "C", None)], None, now + 60).unwrap();

        assert_eq!(report.auto_offered, 0);

        // Near expiry it's replaced and the old one cancelled.
        let report = sync(&api, &fs_, &mut state, &[outbox(1_794_770_000, "C", None)], None, 1_794_770_000 + 86_400 - 3600).unwrap();

        assert_eq!(report.auto_offered, 1);
        assert_eq!(api.cancelled.borrow().as_slice(), ["c1"]);
    }

    #[test]
    fn favourites_newest_side_wins()
    {
        let (_t, fs_) = setup();
        let api = Fake::default();
        let mut state = load_state(&fs_);

        // Starred in the app.
        assert!(set_favorite(&fs_, &mut state, ME, BRAN, true, 1_794_770_000).unwrap());

        let lua = fs::read_to_string(fs_.root(Root::Companion).join("Inbox.lua")).unwrap();

        assert!(lua.contains(&format!("favs = {{ \"{BRAN}\", }}")) && lua.contains("favs_change = 1794770000"));

        // An older list from the game doesn't undo it.
        let mut out = outbox(1_794_770_000, "C", None);

        out.characters[0].favs = Some(Favs { list: vec![ALT.into()], change: 1_794_760_000 });
        sync(&api, &fs_, &mut state, &[out.clone()], None, 1_794_770_100).unwrap();
        assert_eq!(state.favs[ME].list, vec![BRAN.to_string()]);
        // A newer one from the game replaces it, and the inbox follows.
        out.characters[0].favs = Some(Favs { list: vec![ALT.into()], change: 1_794_780_000 });
        sync(&api, &fs_, &mut state, &[out], None, 1_794_780_100).unwrap();
        assert_eq!(state.favs[ME].list, vec![ALT.to_string()]);
        assert_eq!(state.inbox.chars[ME].favs.as_ref().unwrap().list, vec![ALT.to_string()]);
        // A change in the app after that is newer again, even with a slow PC clock.
        set_favorite(&fs_, &mut state, ME, BRAN, true, 1_794_770_000).unwrap();
        assert_eq!(state.favs[ME].change, 1_794_780_001);
        assert_eq!(state.favs[ME].list, vec![ALT.to_string(), BRAN.to_string()]);
    }

    #[test]
    fn at_most_five_favourites()
    {
        let (_t, fs_) = setup();
        let mut state = load_state(&fs_);

        for i in 0..5
        {
            assert!(set_favorite(&fs_, &mut state, ME, &format!("Player-6800-0000000{i}"), true, 1_794_770_000 + i).unwrap());
        }

        assert!(!set_favorite(&fs_, &mut state, ME, "Player-6800-00000009", true, 1_794_770_010).unwrap());
        assert_eq!(state.favs[ME].list.len(), 5);
        set_favorite(&fs_, &mut state, ME, "Player-6800-00000002", false, 1_794_770_020).unwrap();
        assert_eq!(state.favs[ME].list.len(), 4);
    }

    #[test]
    fn no_code_for_partners_without_the_app()
    {
        let (_t, fs_) = setup();
        let api = Fake::default();
        let mut state = load_state(&fs_);
        // Neither paired; Bran hasn't been seen since the update, ALT has no app.
        let mut out = outbox(1_794_770_000, "C", None);

        out.characters[0].partners[1].app = PartnerApp::No;

        let report = sync(&api, &fs_, &mut state, &[out.clone()], None, 1_794_770_100).unwrap();

        assert_eq!(report.auto_offered, 0);
        assert!(api.codes.borrow().is_empty());
        // ALT installs the app: their Sterling says so, and a code follows.
        out.characters[0].partners[1].app = PartnerApp::Has("0.1.9".into());

        let report = sync(&api, &fs_, &mut state, &[out.clone()], None, 1_794_770_200).unwrap();

        assert_eq!(report.auto_offered, 1);
        // And uninstalls: the code is cancelled.
        out.characters[0].partners[1].app = PartnerApp::No;
        sync(&api, &fs_, &mut state, &[out], None, 1_794_770_300).unwrap();
        assert!(state.offers.is_empty());
        assert_eq!(api.cancelled.borrow().len(), 1);
    }

    #[test]
    fn offer_is_dropped_once_paired()
    {
        let (_t, fs_) = setup();
        let api = Fake::default();

        api.pairings.borrow_mut().push(pairing("p1", ME, BRAN, 1_794_000_000));

        let mut state = load_state(&fs_);

        sync(&api, &fs_, &mut state, &[outbox(1_794_770_000, "C", None)], None, 1_794_770_100).unwrap();
        api.pairings.borrow_mut().push(pairing("p2", ME, ALT, 1_794_770_200));
        sync(&api, &fs_, &mut state, &[outbox(1_794_770_000, "C", None)], None, 1_794_770_300).unwrap();
        assert!(state.inbox.chars[ME].offers.is_empty());
        assert!(state.offers.is_empty());
        assert_eq!(api.cancelled.borrow().as_slice(), ["c1"]);
    }

    #[test]
    fn redeems_a_code_from_the_partner_that_sent_it()
    {
        let (_t, fs_) = setup();
        let api = Fake::default();

        api.pairings.borrow_mut().push(pairing("p1", ME, BRAN, 1_794_000_000));

        // ALT's app made a code; ALT's Sterling handed it to us in game.
        let code = api.pair_code(ALT).unwrap().code;
        let mut out = outbox(1_794_770_000, "C", None);

        out.characters[0].codes.insert(ALT.into(), code.clone());

        let mut state = load_state(&fs_);
        let report = sync(&api, &fs_, &mut state, &[out.clone()], None, 1_794_770_100).unwrap();

        assert_eq!(report.auto_paired, vec![(ME.to_string(), ALT.to_string(), "Kaelith Emberfall".to_string())]);
        assert!(report.sent_to.contains(&(ME.to_string(), ALT.to_string())), "copy goes up the same pass");
        assert!(state.inbox.chars[ME].offers.is_empty(), "no offer needed any more");

        let report = sync(&api, &fs_, &mut state, &[out], None, 1_794_770_200).unwrap();

        assert!(report.auto_paired.is_empty());
        assert_eq!(api.redeemed.borrow().len(), 1);
    }

    #[test]
    fn never_redeems_a_code_from_someone_else()
    {
        let (_t, fs_) = setup();
        let api = Fake::default();

        api.pairings.borrow_mut().push(pairing("p1", ME, BRAN, 1_794_000_000));

        // A code that really belongs to a stranger, passed along as if from ALT.
        let code = api.pair_code("Player-6800-DEADBEEF").unwrap().code;
        let mut out = outbox(1_794_770_000, "C", None);

        out.characters[0].codes.insert(ALT.into(), code);

        // And one from someone who isn't a Sterling partner at all.
        let other = api.pair_code("Player-6800-0000FFFF").unwrap().code;

        out.characters[0].codes.insert("Player-6800-0000FFFF".into(), other);

        let mut state = load_state(&fs_);
        let report = sync(&api, &fs_, &mut state, &[out.clone()], None, 1).unwrap();

        assert!(report.auto_paired.is_empty());
        assert!(api.redeemed.borrow().is_empty());
        assert!(report.skipped.iter().any(|s| s.contains("didn't match")));

        // Tried once only.
        let report = sync(&api, &fs_, &mut state, &[out], None, 2).unwrap();

        assert!(!report.skipped.iter().any(|s| s.contains("didn't match")));
    }

    #[test]
    fn auto_pairing_trouble_never_fails_the_sync()
    {
        let (_t, fs_) = setup();
        let api = Fake::default();

        api.pairings.borrow_mut().push(pairing("p1", ME, BRAN, 1_794_000_000));

        let code = api.pair_code(ALT).unwrap().code;

        *api.offline.borrow_mut() = true;

        let mut out = outbox(1_794_770_000, "C", None);

        out.characters[0].codes.insert(ALT.into(), code);

        let mut state = load_state(&fs_);
        let report = sync(&api, &fs_, &mut state, &[out.clone()], None, 1).unwrap();

        assert_eq!(report.uploaded, 2, "normal sync still ran");
        assert!(state.tried.is_empty(), "will try again");
        *api.offline.borrow_mut() = false;

        let report = sync(&api, &fs_, &mut state, &[out], None, 2).unwrap();

        assert_eq!(report.auto_paired.len(), 1);
    }
}
