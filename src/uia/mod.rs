//! UI Automation engine.
//!
//! COM apartment rules force the whole design here: `IUIAutomation` and every
//! element it hands back are single-threaded COM objects that are neither `Send`
//! nor `Sync`, so they can never cross an await point or move between tokio
//! worker threads. We therefore pin all of UIA to one dedicated OS thread that
//! initialises MTA once and lives for the process lifetime; async callers talk
//! to it over a channel and get their answers back through a oneshot.

use anyhow::{anyhow, Result};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::sync::mpsc::{self, Sender};
use tokio::sync::oneshot;

#[cfg(target_os = "macos")]
pub(crate) mod mac;
#[cfg(windows)]
mod win;

/// A top-level window as seen by UI Automation.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct WindowInfo {
    pub name: String,
    pub class_name: String,
    pub control_type: String,
    pub hwnd: isize,
    pub pid: i32,
    pub bounds: Bounds,
    /// The window that owns this one, when it has an owner.
    ///
    /// Present so a dialog can be attributed to the application it is blocking
    /// rather than appearing as an unexplained extra entry: an owned window is
    /// almost always a dialog, and a dialog with an owner is almost always the
    /// answer to "why is that app not responding".
    #[serde(skip_serializing_if = "Option::is_none")]
    pub owned_by: Option<isize>,
}

#[derive(Debug, Clone, Copy, Serialize, JsonSchema)]
pub struct Bounds {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
}

/// One actionable element, with the signed handle needed to act on it.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct Entity {
    pub name: String,
    pub control_type: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub automation_id: String,
    /// Full rect is rarely what a caller needs and doubles the per-entity cost,
    /// so it rides along only in verbose mode. `click_at` is the useful part.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bounds: Option<Bounds>,
    /// Centre point, precomputed so callers never do coordinate maths.
    pub click_at: (i32, i32),
    pub actions: Vec<String>,
    /// Omitted when true - disabled is the exceptional, interesting case.
    #[serde(default, skip_serializing_if = "is_true")]
    pub enabled: bool,
    /// Child-index chain from the window root, relative to `Discovery::scope`.
    /// `act` takes the scope plus this to re-find the element.
    pub path: Vec<u32>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct Discovery {
    pub window: WindowInfo,
    /// Signed, expiring authorisation for this window at this generation.
    /// Pass it back to `act` together with an entity's `path`.
    pub scope: String,
    pub generation: u64,
    pub entities: Vec<Entity>,
    /// Set when a depth or count cap stopped the walk, so a caller is never
    /// silently handed a partial view.
    pub truncated: Option<String>,
    pub elapsed_ms: f64,
}

fn is_true(b: &bool) -> bool {
    *b
}

/// What a caller wants back. `Actionable` is the default because a model asking
/// "what can I do here" is served badly by a wall of layout containers.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Filter {
    /// Things you can actually click, type into, toggle, expand or select.
    Actionable,
    /// Everything the tree filter surfaced that exposes at least one control
    /// pattern, containers and unnamed elements included. Elements with no
    /// pattern at all are never returned: there is nothing to act on and no
    /// action list to hand back.
    All,
}

impl std::str::FromStr for Filter {
    type Err = String;
    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        match s {
            "actionable" => Ok(Filter::Actionable),
            "all" => Ok(Filter::All),
            other => Err(format!("unknown filter '{other}' (actionable|all)")),
        }
    }
}

/// Whether an entity satisfies a selector, and at which tier.
///
/// Mirrors the selector semantics `act` uses, but against an already-returned
/// `Entity` rather than a live COM element - so `wait_for` can poll without a
/// second cross-process walk per candidate. Kept here, and pure, so the
/// semantics can be tested without a desktop.
pub fn entity_matches(e: &Entity, sel: &Selector) -> Option<crate::text::MatchTier> {
    let name_tier = match sel.name.as_ref() {
        None => Some(crate::text::MatchTier::Exact),
        Some(n) => crate::text::tier_of(&e.name, n),
    };
    let ok_id = sel
        .automation_id
        .as_ref()
        .is_none_or(|a| *a == e.automation_id);
    let ok_ct = sel
        .control_type
        .as_ref()
        .is_none_or(|c| c.eq_ignore_ascii_case(&e.control_type));
    match (name_tier, ok_id, ok_ct) {
        (Some(t), true, true) => Some(t),
        _ => None,
    }
}

pub struct DiscoverArgs {
    pub hwnd: Option<isize>,
    pub max_depth: u32,
    pub max_elements: usize,
    pub ttl_secs: u64,
    pub filter: Filter,
    pub verbose: bool,
}

/// What `act` did, and what it saw afterwards.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct ActResult {
    pub ok: bool,
    pub action: String,
    /// `ok` | `identity_changed` | `moved` | `disabled` | `pattern_gone` |
    /// `not_found` | `ambiguous` | `error` | `stopped`
    ///
    /// `not_found` means the target genuinely is not there; `error` means the
    /// lookup itself failed and says nothing about the target. A caller that
    /// retries on one should not retry on the other.
    pub status: String,
    /// How the element was located: "path", "selector" or "ocr".
    pub resolved_by: String,
    /// How closely the caller's name matched the one Windows reported, when a
    /// name was involved at all.
    ///
    /// `exact` and `case` mean the label was found as written. Anything looser
    /// means this crate had to reshape one side to make them meet - folding
    /// Unicode composition, half-width forms, or a localised mnemonic like the
    /// `(F)` in `ファイル(F)` - and a caller pinning down a flaky selector
    /// wants to know that happened.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub matched_by: Option<crate::text::MatchTier>,
    /// A scope for the window as it is *now*, minted after the action.
    ///
    /// Acting frequently changes the very properties the generation hash is
    /// built from - typing into a document puts a modified marker in the title
    /// bar, and the next `act` on the same scope then fails
    /// `identity_changed`. That guard is correct and must stay, but making
    /// every caller re-`discover` after every successful action is a round
    /// trip per keystroke on a heavy app.
    ///
    /// So a successful `act` hands back a fresh scope for the same window.
    /// Paths are unaffected - the tree did not move - so a caller can keep
    /// acting with the paths it already has. Absent when the action failed,
    /// because then the old scope is exactly what should be re-examined.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_scope: Option<String>,
    /// Only on the OCR path, and only for a click that was actually sent.
    ///
    /// A control pattern is a contract with the control; a coordinate click is
    /// a hope about geometry. This is the nearest thing to evidence available:
    /// what the screen did immediately afterwards. `fraction: 0.0` means the
    /// click landed somewhere that did nothing visible - which usually means it
    /// missed, but can also mean the control was already in that state.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub screen_changed: Option<crate::capture::ChangedRegion>,
    pub target: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    pub elapsed_ms: f64,
}

/// Identify an element by what it is rather than where it sat.
///
/// A child-index path is the fastest way to reach an element and the most
/// fragile: expand a tree node between `discover` and `act` and every index
/// after it shifts. The generation guard catches that and refuses, which is
/// correct but is still a failure. A selector is resolved against the live
/// tree, so the same reshape is survivable.
///
/// Deliberately a struct rather than a query language. XPath is an ergonomic
/// for a human writing a test script; for a caller assembling a request it is
/// a string that must be built correctly with no feedback until it fails,
/// where these fields are checked by the schema.
#[derive(Debug, Clone, Default, Deserialize, JsonSchema)]
pub struct Selector {
    /// The visible label, matched leniently but in strict order.
    ///
    /// An exact match is tried first; failing that, Unicode case; failing
    /// that, NFKC and whitespace normalisation, which is what makes `Tệp`
    /// typed on macOS (NFD) meet the same word reported by Windows (NFC) and
    /// full-width `Ａ１` meet `A1`; failing that, decoration is stripped, so
    /// `ファイル` matches the `ファイル(F)` a localised menu actually reports.
    ///
    /// Only the tightest tier that matched anything is kept, so leniency can
    /// never turn a selector that used to resolve one element into
    /// `ambiguous`. The result reports which tier was used as `matched_by`.
    ///
    /// A localised UI is the case this is for, and it is worth preferring
    /// `automation_id` there anyway: labels get translated, identifiers do not.
    pub name: Option<String>,
    /// Exact, case-sensitive, byte for byte - automation ids are identifiers,
    /// not labels, and are stable across locales precisely because no
    /// translator touches them. Prefer this to `name` wherever a control has
    /// one.
    pub automation_id: Option<String>,
    /// As reported by `discover`, e.g. "button", "menu item".
    pub control_type: Option<String>,
}

impl Selector {
    pub fn is_empty(&self) -> bool {
        self.name.is_none() && self.automation_id.is_none() && self.control_type.is_none()
    }

    pub fn describe(&self) -> String {
        let mut parts = Vec::new();
        if let Some(n) = &self.name {
            parts.push(format!("name={n:?}"));
        }
        if let Some(a) = &self.automation_id {
            parts.push(format!("automation_id={a:?}"));
        }
        if let Some(c) = &self.control_type {
            parts.push(format!("control_type={c:?}"));
        }
        parts.join(" ")
    }
}

pub struct ActArgs {
    pub scope: String,
    /// Resolved at act-time instead of trusting the path. Takes precedence
    /// when present.
    pub select: Option<Selector>,
    /// Child-index path from `discover`. Fast, and exact while the tree is
    /// unchanged.
    pub path: Vec<u32>,
    pub action: String,
    pub value: Option<String>,
}

/// Bounds of a single top-level window, for OCR's region-of-interest path.
/// Uses the Win32 rect rather than a UIA query: it needs no COM thread, and a
/// window's outer rectangle is exactly what we want to crop to.
#[cfg(windows)]
pub fn window_bounds(hwnd: isize) -> Result<Bounds> {
    use windows::Win32::Foundation::{HWND, RECT};
    use windows::Win32::UI::WindowsAndMessaging::GetWindowRect;
    let h = HWND(hwnd as *mut core::ffi::c_void);
    let mut r = RECT::default();
    unsafe { GetWindowRect(h, &mut r) }.map_err(|e| anyhow!("GetWindowRect({hwnd}): {e}"))?;
    Ok(Bounds {
        x: r.left,
        y: r.top,
        w: r.right - r.left,
        h: r.bottom - r.top,
    })
}

/// Commands the COM thread understands. Each carries its own reply channel.
enum Cmd {
    ListWindows(oneshot::Sender<Result<Vec<WindowInfo>>>),
    Discover(DiscoverArgs, oneshot::Sender<Result<Discovery>>),
    Act(ActArgs, oneshot::Sender<Result<ActResult>>),
}

/// Handle to the UIA thread. Cloneable; the thread stops when all handles drop
/// and the channel closes.
#[derive(Clone)]
pub struct Engine {
    tx: Sender<Cmd>,
}

/// HMAC key for lease signing, loaded once and shared with the COM thread.
pub struct EngineConfig {
    pub lease_key: Vec<u8>,
}

impl Engine {
    /// Start the COM thread. Returns once the thread has successfully created
    /// its `IUIAutomation` instance, so a failure here is reported to the
    /// caller rather than surfacing later as a mysterious timeout.
    pub fn spawn(cfg: EngineConfig) -> Result<Self> {
        let (tx, rx) = mpsc::channel::<Cmd>();
        let (ready_tx, ready_rx) = mpsc::channel::<Result<()>>();

        std::thread::Builder::new()
            .name("wincrust-uia".into())
            .spawn(move || {
                #[cfg(windows)]
                win::run(rx, ready_tx, cfg);
                #[cfg(target_os = "macos")]
                mac::run(rx, ready_tx, cfg);
                #[cfg(not(any(windows, target_os = "macos")))]
                {
                    let _ = (rx, cfg);
                    let _ = ready_tx.send(Err(anyhow!("wincrust requires Windows")));
                }
            })?;

        ready_rx
            .recv()
            .map_err(|_| anyhow!("UIA thread died during startup"))??;

        Ok(Self { tx })
    }

    pub async fn discover(&self, args: DiscoverArgs) -> Result<Discovery> {
        let (rtx, rrx) = oneshot::channel();
        self.tx
            .send(Cmd::Discover(args, rtx))
            .map_err(|_| anyhow!("UIA thread is gone"))?;
        rrx.await
            .map_err(|_| anyhow!("UIA thread dropped the reply"))?
    }

    pub async fn act(&self, args: ActArgs) -> Result<ActResult> {
        let (rtx, rrx) = oneshot::channel();
        self.tx
            .send(Cmd::Act(args, rtx))
            .map_err(|_| anyhow!("UIA thread is gone"))?;
        rrx.await
            .map_err(|_| anyhow!("UIA thread dropped the reply"))?
    }

    pub async fn list_windows(&self) -> Result<Vec<WindowInfo>> {
        let (rtx, rrx) = oneshot::channel();
        self.tx
            .send(Cmd::ListWindows(rtx))
            .map_err(|_| anyhow!("UIA thread is gone"))?;
        rrx.await
            .map_err(|_| anyhow!("UIA thread dropped the reply"))?
    }
}

// Engine is Clone and deliberately has no Drop impl. Signalling shutdown from
// one would be a per-clone action with a process-wide effect: the HTTP transport
// builds a Wincrust (and so an Engine clone) per session, so the first session to
// end would take down the COM thread every other session shares. The thread
// already stops on its own - `rx.recv()` returns Err once the last Sender is
// dropped, which is exactly the "no owners left" condition wanted.

#[cfg(test)]
mod selector_tests {
    use super::*;

    #[test]
    fn empty_selector_is_recognised() {
        assert!(Selector::default().is_empty());
        assert!(!Selector {
            name: Some("Save".into()),
            ..Default::default()
        }
        .is_empty());
    }

    /// The description ends up in an `ambiguous` or `not_found` message, which
    /// is the caller's only clue about what to narrow.
    #[test]
    fn describes_every_field_it_was_given() {
        let s = Selector {
            name: Some("Save".into()),
            automation_id: Some("btnSave".into()),
            control_type: Some("button".into()),
        };
        let d = s.describe();
        assert!(
            d.contains("Save") && d.contains("btnSave") && d.contains("button"),
            "{d}"
        );
    }

    #[test]
    fn describes_only_what_was_set() {
        let d = Selector {
            control_type: Some("button".into()),
            ..Default::default()
        }
        .describe();
        assert_eq!(d, "control_type=\"button\"");
    }
}

#[cfg(test)]
mod wait_match_tests {
    use super::*;
    use crate::text::MatchTier;

    fn ent(name: &str, ct: &str, aid: &str, enabled: bool) -> Entity {
        Entity {
            name: name.into(),
            control_type: ct.into(),
            automation_id: aid.into(),
            bounds: None,
            click_at: (0, 0),
            actions: vec![],
            enabled,
            path: vec![],
        }
    }

    #[test]
    fn an_empty_selector_matches_anything() {
        let e = ent("Save", "button", "", true);
        assert_eq!(
            entity_matches(&e, &Selector::default()),
            Some(MatchTier::Exact)
        );
    }

    #[test]
    fn it_uses_the_same_locale_ladder_as_act() {
        // If wait_for matched differently from act, waiting for a control and
        // then acting on it could disagree - which is the worst possible bug
        // in a wait primitive.
        let e = ent("\u{30d5}\u{30a1}\u{30a4}\u{30eb}(F)", "menu item", "", true);
        let sel = Selector {
            name: Some("\u{30d5}\u{30a1}\u{30a4}\u{30eb}".into()),
            ..Default::default()
        };
        assert_eq!(entity_matches(&e, &sel), Some(MatchTier::Affix));
    }

    #[test]
    fn automation_id_is_exact_and_case_sensitive() {
        let e = ent("Save", "button", "btnSave", true);
        let ok = Selector {
            automation_id: Some("btnSave".into()),
            ..Default::default()
        };
        let bad = Selector {
            automation_id: Some("btnsave".into()),
            ..Default::default()
        };
        assert!(entity_matches(&e, &ok).is_some());
        assert!(entity_matches(&e, &bad).is_none());
    }

    #[test]
    fn every_named_field_must_match() {
        let e = ent("Save", "button", "btnSave", true);
        let wrong_type = Selector {
            name: Some("Save".into()),
            control_type: Some("menu item".into()),
            ..Default::default()
        };
        assert!(entity_matches(&e, &wrong_type).is_none());
    }
}
