//! macOS accessibility backend. Native references remain on the engine thread.
//! Window IDs are process-local opaque handles, not CGWindowIDs or pointers.
use super::*;
use crate::lease::{now, Scope};
use anyhow::{bail, ensure};
use objc2::rc::autoreleasepool;
use objc2_app_kit::NSWorkspace;
use objc2_application_services::{AXError, AXIsProcessTrusted, AXUIElement, AXValue, AXValueType};
use objc2_core_foundation::{CFArray, CFBoolean, CFRetained, CFString, CFType, CGPoint, CGSize};
use std::collections::HashMap;
use std::ptr::NonNull;
use std::sync::mpsc::{Receiver, Sender};
use std::time::{Duration, Instant};

/// Wall-clock ceiling on one operation - enumeration and walk together, not
/// each. An unresponsive app must not hold the single engine thread, and every
/// other caller queued behind it, for longer than this.
const WALK_BUDGET: Duration = Duration::from_secs(5);

/// Ceiling on a single AX request, so the budget above is enforceable at all.
///
/// A responsive application answers an attribute read in tens of microseconds,
/// so this is four orders of magnitude of headroom; the default is six seconds,
/// and `entity()` makes fourteen requests per node.
const AX_REQUEST_TIMEOUT: f32 = 0.5;

/// Floor for the same, because the system-wide object reads a timeout of zero
/// as "reset to the default" - the opposite of what shrinking it means here.
const AX_MIN_TIMEOUT: f32 = 0.05;

/// Upper bound on the AX requests one node costs: role, actions, four
/// settability probes, four name candidates, identifier, position, size,
/// enabled, and the child fetch. The deadline is only checked *between* nodes,
/// so the per-request cap has to be the remaining budget divided by this or a
/// single unresponsive node walks straight through it - measured at 8.2s
/// against a 5s budget when the cap was sized to the request instead.
const AX_REQUESTS_PER_NODE: f32 = 16.0;

fn check(e: AXError) -> Result<()> {
    ensure!(e == AXError::Success, "macOS accessibility error: {e:?}");
    Ok(())
}

pub(crate) fn trusted() -> bool {
    // Query only: permission prompts must be an explicit setup action.
    unsafe { AXIsProcessTrusted() }
}

fn attr(el: &AXUIElement, name: &str) -> Result<CFRetained<CFType>> {
    let mut raw = std::ptr::null();
    unsafe {
        check(el.copy_attribute_value(&CFString::from_str(name), NonNull::from(&mut raw)))?;
        let ptr =
            NonNull::new(raw.cast_mut()).ok_or_else(|| anyhow!("empty AX attribute {name}"))?;
        Ok(CFRetained::from_raw(ptr))
    }
}

fn string(el: &AXUIElement, name: &str) -> String {
    attr(el, name)
        .ok()
        .and_then(|v| v.downcast::<CFString>().ok())
        .map(|v| v.to_string())
        .unwrap_or_default()
}

fn boolean(el: &AXUIElement, name: &str) -> Option<bool> {
    attr(el, name)
        .ok()?
        .downcast::<CFBoolean>()
        .ok()
        .map(|b| b.value())
}

fn elements(el: &AXUIElement, name: &str) -> Result<Vec<CFRetained<AXUIElement>>> {
    let v = attr(el, name)?;
    let array = v
        .downcast::<CFArray>()
        .map_err(|_| anyhow!("{name} is not an array"))?;
    // AX arrays contain CF objects; still validate every element's runtime type.
    let values = unsafe { array.cast_unchecked::<CFType>() }.to_vec();
    values
        .into_iter()
        .map(|v| {
            v.downcast::<AXUIElement>()
                .map_err(|_| anyhow!("{name} contains a non-element"))
        })
        .collect()
}

fn settable(el: &AXUIElement, name: &str) -> bool {
    let mut value = 0;
    unsafe {
        el.is_attribute_settable(&CFString::from_str(name), NonNull::from(&mut value))
            == AXError::Success
            && value != 0
    }
}

fn set(el: &AXUIElement, name: &str, value: &CFType) -> Result<()> {
    unsafe { check(el.set_attribute_value(&CFString::from_str(name), value)) }
}

fn press(el: &AXUIElement, action: &str) -> Result<()> {
    unsafe { check(el.perform_action(&CFString::from_str(action))) }
}

fn actions(el: &AXUIElement) -> Vec<String> {
    let mut raw = std::ptr::null();
    unsafe {
        if el.copy_action_names(NonNull::from(&mut raw)) != AXError::Success {
            return vec![];
        }
        let Some(ptr) = NonNull::new(raw.cast_mut()) else {
            return vec![];
        };
        let a = CFRetained::from_raw(ptr);
        a.cast_unchecked::<CFType>()
            .to_vec()
            .into_iter()
            .filter_map(|v| v.downcast::<CFString>().ok())
            .map(|v| v.to_string())
            .collect()
    }
}

pub(super) fn bounds(el: &AXUIElement) -> Result<Bounds> {
    let p = attr(el, "AXPosition")?
        .downcast::<AXValue>()
        .map_err(|_| anyhow!("invalid AXPosition"))?;
    let s = attr(el, "AXSize")?
        .downcast::<AXValue>()
        .map_err(|_| anyhow!("invalid AXSize"))?;
    let mut point = CGPoint::default();
    let mut size = CGSize::default();
    unsafe {
        ensure!(
            p.value(AXValueType::CGPoint, NonNull::from(&mut point).cast()),
            "invalid position"
        );
        ensure!(
            s.value(AXValueType::CGSize, NonNull::from(&mut size).cast()),
            "invalid size"
        );
    }
    Ok(Bounds {
        x: point.x.round() as i32,
        y: point.y.round() as i32,
        w: size.width.round() as i32,
        h: size.height.round() as i32,
    })
}

/// Longest name this backend will report.
///
/// AXValue is the last name fallback, and for a text area AXValue is the entire
/// document. An unlabelled terminal or editor pane therefore hands back its
/// whole scrollback as a control's "name": measured at 2,236,997 characters for
/// one Warp window, which turned a *five-entity* discover into 2.4 MB of JSON
/// and roughly 600,000 tokens - past any context window, and orders of
/// magnitude worse than the screenshot this exists to replace.
///
/// Windows cannot reach this state: it names controls from the UIA Name
/// property and never falls back to a value.
const MAX_NAME_CHARS: usize = 120;

/// Bound a name at a character boundary, marking that something was cut.
///
/// Char-indexed rather than byte-indexed because these names are routinely not
/// ASCII - a Japanese menu label would panic a byte truncate.
fn clip(mut s: String) -> String {
    if let Some((end, _)) = s.char_indices().nth(MAX_NAME_CHARS) {
        s.truncate(end);
        s.push('\u{2026}');
    }
    s
}

fn entity(el: &AXUIElement, path: Vec<u32>, verbose: bool) -> Entity {
    let role = string(el, "AXRole");
    let role = match role.as_str() {
        "AXButton" => "button",
        "AXTextField" | "AXTextArea" | "AXComboBox" => "edit",
        "AXCheckBox" => "check box",
        "AXRadioButton" => "radio button",
        "AXMenuItem" => "menu item",
        "AXMenuBar" => "menu bar",
        "AXWindow" => "window",
        "AXTabGroup" => "tab",
        "AXLink" => "link",
        "AXRow" => "row",
        "AXStaticText" => "text",
        other => other,
    }
    .to_string();
    let native = actions(el);
    let mut supported = Vec::new();
    if native.iter().any(|a| a == "AXPress") {
        supported.push("click".into());
    }
    if role == "edit" && settable(el, "AXValue") {
        supported.push("type".into());
    }
    if settable(el, "AXExpanded") {
        supported.push("expand".into());
    }
    if settable(el, "AXSelected") {
        supported.push("select".into());
    }
    if role == "check box" && native.iter().any(|a| a == "AXPress") {
        supported.push("toggle".into());
    }
    if native.iter().any(|a| a == "AXRaise") {
        supported.push("raise".into());
    }
    if settable(el, "AXFocused") || role == "window" {
        supported.extend(["key".into(), "type_keys".into()]);
    }
    let name = clip(
        ["AXTitle", "AXDescription", "AXHelp", "AXValue"]
            .into_iter()
            .map(|n| string(el, n))
            .find(|s| !s.is_empty())
            .unwrap_or_default(),
    );
    let b = bounds(el).ok();
    Entity {
        name,
        control_type: role,
        automation_id: string(el, "AXIdentifier"),
        bounds: if verbose { b } else { None },
        click_at: b
            .map(|b| (b.x + b.w / 2, b.y + b.h / 2))
            .unwrap_or_default(),
        actions: supported,
        // Absent means "not a control", not "disabled". Windows' IsEnabled is
        // always present; AXEnabled is published only by things that can be
        // greyed out, so text areas, scroll areas, groups and windows omit it.
        // Defaulting those to false made `act` refuse every one of them.
        enabled: boolean(el, "AXEnabled").unwrap_or(true),
        path,
    }
}

struct Window {
    el: CFRetained<AXUIElement>,
    pid: i32,
    app: String,
}
struct Snapshot {
    hwnd: isize,
    expires: u64,
    nodes: Vec<(CFRetained<AXUIElement>, Entity)>,
}
struct Desktop {
    windows: HashMap<isize, Window>,
    next_id: isize,
    next_generation: u64,
    snapshots: HashMap<u64, Snapshot>,
    key: Vec<u8>,
    /// Held rather than recreated because it is the only handle through which
    /// a request timeout can be set process-wide. See `cap`.
    system: CFRetained<AXUIElement>,
}

impl Desktop {
    /// Cap every subsequent AX request from this process at `seconds`.
    ///
    /// Only the system-wide object does this globally. Setting a timeout on an
    /// application element binds *that reference* and nothing else - not the
    /// windows it returned, not its children, not an equal element created
    /// later. So the per-application call this replaces bounded exactly one
    /// request, `AXWindows` on a temporary handle, and left the tree walk and
    /// `act`'s path re-walk running on the six-second default.
    ///
    /// Deadline checks between units of work bound the total; this bounds the
    /// unit. Neither works without the other: a check between nodes cannot
    /// interrupt a node already blocked inside a request.
    fn cap(&self, seconds: f32) {
        unsafe {
            self.system
                .set_messaging_timeout(seconds.clamp(AX_MIN_TIMEOUT, AX_REQUEST_TIMEOUT));
        }
    }

    fn list(&mut self, deadline: Instant) -> Result<Vec<WindowInfo>> {
        ensure!(trusted(), "Accessibility permission missing. Grant the host application or installed Wincrust executable access in System Settings > Privacy & Security > Accessibility, then restart it.");
        self.cap(AX_REQUEST_TIMEOUT);
        let apps = NSWorkspace::sharedWorkspace().runningApplications();
        // Cheap and AX-free: who is running at all, as opposed to who answered.
        let running: Vec<i32> = apps.iter().map(|a| a.processIdentifier()).collect();
        let mut live = Vec::new();
        let mut asked = Vec::new();
        for app in apps {
            if Instant::now() >= deadline {
                tracing::warn!(
                    "window enumeration hit the {}s budget after {} of {} applications",
                    WALK_BUDGET.as_secs(),
                    asked.len(),
                    running.len()
                );
                break;
            }
            let pid = app.processIdentifier();
            let root = unsafe { AXUIElement::new_application(pid) };
            let Ok(windows) = elements(&root, "AXWindows") else {
                continue;
            };
            asked.push(pid);
            for el in windows {
                let id = self
                    .windows
                    .iter()
                    .find(|(_, w)| w.pid == pid && *w.el == *el)
                    .map(|(id, _)| *id)
                    .unwrap_or_else(|| {
                        self.next_id += 1;
                        self.next_id
                    });
                self.windows.insert(
                    id,
                    Window {
                        el,
                        pid,
                        app: app
                            .bundleIdentifier()
                            .map(|v| v.to_string())
                            .unwrap_or_default(),
                    },
                );
                live.push(id);
            }
        }
        // Forget a window only when the application it belongs to answered and
        // did not list it, or when that application is gone. An application
        // that was unreachable, or that the budget cut us off before reaching,
        // keeps its windows: evicting them would invalidate outstanding scopes
        // and report "window closed" for a window that is still on screen.
        self.windows.retain(|id, w| {
            running.contains(&w.pid) && (live.contains(id) || !asked.contains(&w.pid))
        });
        Ok(live
            .into_iter()
            .filter_map(|id| self.info(id).ok())
            .collect())
    }

    fn info(&self, id: isize) -> Result<WindowInfo> {
        let w = self
            .windows
            .get(&id)
            .ok_or_else(|| anyhow!("window no longer exists"))?;
        Ok(WindowInfo {
            name: string(&w.el, "AXTitle"),
            class_name: w.app.clone(),
            control_type: "window".into(),
            hwnd: id,
            pid: w.pid,
            bounds: bounds(&w.el)?,
            owned_by: None,
        })
    }

    fn discover(&mut self, a: DiscoverArgs) -> Result<Discovery> {
        let start = Instant::now();
        let deadline = start + WALK_BUDGET;
        self.list(deadline)?;
        let id = match a.hwnd {
            Some(id) => id,
            None => {
                let app = NSWorkspace::sharedWorkspace()
                    .frontmostApplication()
                    .ok_or_else(|| anyhow!("no foreground application"))?;
                let root = unsafe { AXUIElement::new_application(app.processIdentifier()) };
                let focused = attr(&root, "AXFocusedWindow")?
                    .downcast::<AXUIElement>()
                    .map_err(|_| anyhow!("no focused window"))?;
                *self
                    .windows
                    .iter()
                    .find(|(_, w)| *w.el == *focused)
                    .ok_or_else(|| anyhow!("focused window unavailable"))?
                    .0
            }
        };
        let w = self
            .windows
            .get(&id)
            .ok_or_else(|| anyhow!("unknown window ID; call windows again"))?;
        let mut nodes = Vec::new();
        let mut pending = vec![(w.el.clone(), vec![])];
        let mut truncated = None;
        let cap = a.max_elements.clamp(1, 2000);
        // Name the limit that actually fired, and say how much is left behind.
        // A caller that is only told "truncated" cannot tell a small window from
        // a cut one, and the natural recovery - act on what you can see - is
        // exactly how the wrong control gets clicked. `entities` is the filtered
        // count, so `examined` is what the caps are really measured against.
        while let Some((el, path)) = pending.pop() {
            if nodes.len() >= cap {
                truncated = Some(format!(
                    "element cap {cap} reached; examined {}, at least {} not visited",
                    nodes.len(),
                    pending.len() + 1
                ));
                break;
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                truncated = Some(format!(
                    "time budget {}ms reached; examined {}, at least {} not visited",
                    WALK_BUDGET.as_millis(),
                    nodes.len(),
                    pending.len() + 1
                ));
                break;
            }
            self.cap(remaining.as_secs_f32() / AX_REQUESTS_PER_NODE);
            let ent = entity(&el, path.clone(), a.verbose);
            nodes.push((el.clone(), ent));
            if path.len() >= a.max_depth.min(64) as usize {
                truncated = Some(format!("depth cap {} reached", a.max_depth));
                continue;
            }
            if let Ok(children) = elements(&el, "AXChildren") {
                for (i, child) in children.into_iter().enumerate().rev() {
                    let mut p = path.clone();
                    p.push(i as u32);
                    pending.push((child, p));
                }
            }
        }
        self.snapshots.retain(|_, s| s.expires >= now());
        // Bounded storage: Mac AX objects have no public, durable identifier.
        if self.snapshots.len() >= 64 {
            if let Some(old) = self.snapshots.keys().min().copied() {
                self.snapshots.remove(&old);
            }
        }
        self.next_generation += 1;
        let generation = self.next_generation;
        let expires = now() + a.ttl_secs.clamp(1, 300);
        let entities = nodes
            .iter()
            .filter(|(_, e)| a.filter == Filter::All || !e.actions.is_empty())
            .map(|(_, e)| e.clone())
            .collect();
        let scope = Scope {
            hwnd: id,
            generation,
            exp: expires,
        }
        .encode(&self.key)?;
        self.snapshots.insert(
            generation,
            Snapshot {
                hwnd: id,
                expires,
                nodes,
            },
        );
        let window = self.info(id).map_err(|e| {
            anyhow!("window is unreadable; its application may be unresponsive ({e})")
        })?;
        Ok(Discovery {
            window,
            scope,
            generation,
            entities,
            truncated,
            elapsed_ms: start.elapsed().as_secs_f64() * 1000.0,
        })
    }

    fn act(&mut self, a: ActArgs) -> Result<ActResult> {
        let start = Instant::now();
        let mut result = ActResult {
            ok: false,
            action: a.action.clone(),
            status: "error".into(),
            resolved_by: if a.select.is_some() {
                "selector"
            } else {
                "path"
            }
            .into(),
            matched_by: None,
            next_scope: None,
            screen_changed: None,
            target: String::new(),
            detail: None,
            elapsed_ms: 0.0,
        };
        let outcome = self.act_inner(&a, &mut result);
        if let Err(e) = outcome {
            result.detail = Some(e.to_string());
        }
        result.elapsed_ms = start.elapsed().as_secs_f64() * 1000.0;
        Ok(result)
    }

    fn act_inner(&mut self, a: &ActArgs, r: &mut ActResult) -> Result<()> {
        ensure!(trusted(), "Accessibility permission missing");
        // discover leaves the cap wherever its budget ran down to.
        self.cap(AX_REQUEST_TIMEOUT);
        if crate::guard::engaged() {
            r.status = "stopped".into();
            bail!(crate::guard::refusal());
        }
        let scope = match Scope::decode(&a.scope, &self.key) {
            Ok(s) => s,
            Err(e) => {
                r.status = "identity_changed".into();
                return Err(e);
            }
        };
        let snapshot = self
            .snapshots
            .get(&scope.generation)
            .filter(|s| s.hwnd == scope.hwnd)
            .ok_or_else(|| {
                r.status = "identity_changed".into();
                anyhow!("snapshot expired or evicted; discover again")
            })?;
        let (old, expected) = if let Some(sel) = &a.select {
            let mut hits: Vec<_> = snapshot
                .nodes
                .iter()
                .filter_map(|(el, e)| entity_matches(e, sel).map(|t| (el, e, t)))
                .collect();
            crate::text::keep_best(&mut hits, |h| h.2);
            if hits.len() != 1 {
                r.status = if hits.is_empty() {
                    "not_found"
                } else {
                    "ambiguous"
                }
                .into();
                bail!(
                    "selector resolved {} elements; discover again or narrow it",
                    hits.len()
                );
            }
            let (el, e, t) = hits.remove(0);
            r.matched_by = Some(t);
            (el.clone(), e.clone())
        } else {
            snapshot
                .nodes
                .iter()
                .find(|(_, e)| e.path == a.path)
                .cloned()
                .ok_or_else(|| {
                    r.status = "not_found".into();
                    anyhow!("path was not observed")
                })?
        };
        // Re-walk the path and compare the retained AX object's identity. A sibling
        // replacing this index must never inherit the old control's authority.
        let window = self
            .windows
            .get(&scope.hwnd)
            .ok_or_else(|| anyhow!("window closed"))?;
        let mut live = window.el.clone();
        for index in &expected.path {
            live = elements(&live, "AXChildren")?
                .get(*index as usize)
                .cloned()
                .ok_or_else(|| anyhow!("path changed; discover again"))?;
        }
        let current = entity(&live, expected.path.clone(), true);
        r.target = current.name.clone();
        if *live != *old
            || current.name != expected.name
            || current.control_type != expected.control_type
            || current.automation_id != expected.automation_id
        {
            r.status = "identity_changed".into();
            bail!("element changed; discover again");
        }
        if !current.enabled {
            r.status = "disabled".into();
            bail!("control reports AXEnabled=false");
        }
        if !current.actions.contains(&a.action) {
            r.status = "pattern_gone".into();
            bail!("action not supported; use actions returned by discover");
        }
        if crate::guard::engaged() {
            r.status = "stopped".into();
            bail!(crate::guard::refusal());
        }
        let mut note: Option<&str> = None;
        match a.action.as_str() {
            "click" | "toggle" => press(&live, "AXPress")?,
            "raise" => press(&live, "AXRaise")?,
            "type" => set(
                &live,
                "AXValue",
                &CFString::from_str(
                    a.value
                        .as_deref()
                        .ok_or_else(|| anyhow!("type requires value"))?,
                ),
            )?,
            "expand" | "select" => set(
                &live,
                if a.action == "expand" {
                    "AXExpanded"
                } else {
                    "AXSelected"
                },
                CFBoolean::new(true),
            )?,
            "key" | "type_keys" => {
                let value = a
                    .value
                    .as_deref()
                    .ok_or_else(|| anyhow!("keyboard action requires value"))?;
                // Validate the complete sequence before focus changes or input.
                let keys = if a.action == "key" {
                    Some(crate::macos::input::prepare(value)?)
                } else {
                    crate::macos::input::validate_text(value)?;
                    None
                };
                if current.control_type != "window" {
                    set(&live, "AXFocused", CFBoolean::new(true))?;
                    ensure!(
                        boolean(&live, "AXFocused") == Some(true),
                        "control did not accept focus; no keyboard input sent"
                    );
                } else {
                    let app = unsafe { AXUIElement::new_application(window.pid) };
                    let focused = attr(&app, "AXFocusedWindow")?
                        .downcast::<AXUIElement>()
                        .map_err(|_| anyhow!("no focused window"))?;
                    ensure!(
                        *focused == *live,
                        "target is not this application's focused window; no keyboard input sent"
                    );
                }
                if let Some(keys) = keys {
                    // Observed on TextEdit: a chord posted to a background app
                    // is delivered and dropped, because a key equivalent is
                    // matched by the *active* application. Unicode text is
                    // inserted either way. The action still runs - there is no
                    // activate verb to recover with - but "ok" here means
                    // dispatched, and a caller re-reading an unchanged UI
                    // deserves to know which of the two cases it is in.
                    if NSWorkspace::sharedWorkspace()
                        .frontmostApplication()
                        .map(|app| app.processIdentifier())
                        != Some(window.pid)
                    {
                        note = Some(
                            "The target application was not frontmost; macOS may have discarded \
                             this chord. Re-read the UI, or use type_keys, which is unaffected.",
                        );
                    }
                    crate::macos::input::send_prepared(window.pid, &keys)?;
                } else {
                    crate::macos::input::send_text(window.pid, value)?;
                }
            }
            _ => bail!("unsupported action"),
        }
        r.ok = true;
        r.status = "ok".into();
        let dispatched = "Native action dispatched. Discover again to verify the resulting UI and obtain a fresh scope.";
        r.detail = Some(match note {
            Some(n) => format!("{dispatched} {n}"),
            None => dispatched.into(),
        });
        // Do not mint a scope claiming that the pre-action paths are still current.
        Ok(())
    }
}

pub(super) fn run(rx: Receiver<Cmd>, ready: Sender<Result<()>>, cfg: EngineConfig) {
    let mut desktop = Desktop {
        windows: HashMap::new(),
        next_id: 0,
        next_generation: 0,
        snapshots: HashMap::new(),
        key: cfg.lease_key,
        system: unsafe { AXUIElement::new_system_wide() },
    };
    desktop.cap(AX_REQUEST_TIMEOUT);
    let _ = ready.send(Ok(()));
    for command in rx {
        autoreleasepool(|_| match command {
            Cmd::ListWindows(reply) => {
                let _ = reply.send(desktop.list(Instant::now() + WALK_BUDGET));
            }
            Cmd::Discover(args, reply) => {
                let _ = reply.send(desktop.discover(args));
            }
            Cmd::Act(args, reply) => {
                let _ = reply.send(desktop.act(args));
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::{clip, MAX_NAME_CHARS};

    /// The case that motivated the cap: a text area reports its whole document
    /// as AXValue, and AXValue is the last name fallback.
    #[test]
    fn a_scrollback_sized_value_does_not_become_a_name() {
        let huge = "x".repeat(2_236_997);
        let out = clip(huge);
        assert_eq!(
            out.chars().count(),
            MAX_NAME_CHARS + 1,
            "cap plus the ellipsis"
        );
        assert!(out.ends_with('\u{2026}'));
    }

    /// Names are routinely not ASCII, and a byte truncate would panic here.
    #[test]
    fn it_cuts_on_character_boundaries() {
        let out = clip("\u{6f22}".repeat(MAX_NAME_CHARS * 2));
        assert_eq!(out.chars().count(), MAX_NAME_CHARS + 1);
        assert!(out.starts_with('\u{6f22}'));
    }

    #[test]
    fn a_short_name_is_untouched() {
        assert_eq!(clip("Save".into()), "Save");
        let exact = "y".repeat(MAX_NAME_CHARS);
        assert_eq!(clip(exact.clone()), exact, "no ellipsis at exactly the cap");
    }
}
