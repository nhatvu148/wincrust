//! The MCP surface.
//!
//! Seven tools, and deliberately nothing that generalises. This process runs at
//! High integrity and is reachable over the network, so every tool is attack
//! surface: there is no shell, no registry, no filesystem, and no way to run an
//! arbitrary command - `launch` starts only what the host's allowlist names, by
//! exact match. Anything outside "look at the desktop and act on a control"
//! belongs on the SSH side, where it is not running with an admin token.
//!
//! The count is incidental; the rule is that a tool earns its place by being
//! narrow. `launch` earned one because starting a named application cannot be
//! expressed as looking at the desktop; `wait_for` earned one because polling
//! `discover` from outside costs a round trip per attempt and races whatever
//! it is waiting for. Neither widened what the server can reach.

use crate::{capture, guard, ocr, uia};
use rmcp::handler::server::wrapper::{Json, Parameters};
use rmcp::model::{
    CallToolResult, ContentBlock, Implementation, ProtocolVersion, ServerCapabilities, ServerInfo,
};
use rmcp::{tool, tool_handler, tool_router, ErrorData as McpError, ServerHandler};
use schemars::JsonSchema;
use serde::Deserialize;
use serde::Serialize;

#[derive(Clone)]
pub struct Wincrust {
    engine: uia::Engine,
    allowlist: std::sync::Arc<Vec<String>>,
}

#[derive(Debug, Deserialize, JsonSchema, Default)]
pub struct DiscoverParams {
    /// Window handle from `windows`. Omit to use the foreground window.
    pub hwnd: Option<isize>,
    /// "actionable" (default) or "all".
    pub filter: Option<String>,
    /// Cap on returned entities. Default 400.
    pub max_elements: Option<usize>,
    /// macOS: how deep to walk the application's menu bar, where most of a Mac
    /// app's capability lives.
    ///
    /// Default 1, which names the top-level menus - File, Edit, View - for
    /// about 15% more response. Set 3 to see every command inside them, which
    /// costs roughly four times the whole response and is worth paying once you
    /// know you need a menu command. 0 skips menus entirely.
    pub menu_depth: Option<u32>,
}

/// Names the menus without pricing every discover like a menu dump.
///
/// Depth 3 - the level that holds the actual commands - took a Chrome window
/// from 2,074 to 9,099 tokens, on every observation, which would undo the
/// reason a semantic backend beats a screenshot. Depth 1 costs 332 tokens,
/// tells a caller which menus exist, and lets it ask for the contents of the
/// one it wants.
pub const DEFAULT_MENU_DEPTH: u32 = 1;

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ActParams {
    /// The `scope` string from the discover response that produced this entity.
    pub scope: String,
    /// Resolve by identity instead of position: the durable option, and the
    /// one to reach for first. Survives a tree reshape that a path does not.
    /// Ambiguity is an error, so be specific.
    pub select: Option<uia::Selector>,
    /// The entity's `path`. Fast and exact while the tree is unchanged - but
    /// it is an index into the tree as `discover` saw it, and a tooltip
    /// appearing is enough to shift every index by one. Best used immediately;
    /// for anything that waits, or that runs after the screen may have moved,
    /// `select` is what survives.
    #[serde(default)]
    pub path: Vec<u32>,
    /// Fall back to OCR when the selector finds nothing in the UI tree.
    ///
    /// Off by default, and that is deliberate. An OCR hit is a rectangle, not
    /// a control: there is no pattern to invoke, so acting on one means moving
    /// the real cursor and clicking whatever is topmost at that point. That
    /// gives up every guarantee the pattern path provides. Turn it on only for
    /// applications that draw their own interface and expose no tree - the
    /// result will say `resolved_by: "ocr"` when this path was taken.
    #[serde(default)]
    pub allow_ocr: bool,
    // The routing line leads because this description is long and a reader who
    // pattern-matches on the first plausible verb stops there. One did: `type`
    // failed for want of a value pattern, `key` refused the ':' in a path, and
    // the workaround was a .cmd renamed to pure letters so it could be spelled
    // out a keystroke at a time. `type_keys` was documented the whole time,
    // three paragraphs down - which is the same as not being documented. Put
    // the answer above the explanation, and resist lengthening either.
    /// click | type | type_keys | key | toggle | expand | select
    ///
    /// Which one: `type` for form fields. `type_keys` for consoles, terminals,
    /// and any text containing ':' or '\'. `key` for Enter, Escape and chords
    /// only.
    ///
    /// `type` sets a field's value through a control pattern. `key` sends
    /// keystrokes - which is a different thing, and the one you need for
    /// anything a value cannot express: Enter to submit, Escape to dismiss,
    /// Ctrl+S to save. A console prompt is a text field *plus* Enter, so it
    /// usually takes both.
    ///
    /// `type_keys` is for the surfaces `type` cannot reach. A console,
    /// a terminal, and most custom-drawn text areas expose no value pattern
    /// at all, so `type` fails on them permanently rather than transiently.
    /// `type_keys` sends the same string as characters instead, which means it
    /// can type ':' and '\' - so, unlike `key`, it can type a file path. The
    /// cost is that it takes focus and delivers to whatever holds it, so it
    /// reports that the text was sent, not that the control received it.
    pub action: String,
    /// Text for `type` and `type_keys`; a key specification for `key`.
    ///
    /// Key specs are whitespace-separated chords: `Enter`, `Ctrl+S`,
    /// `Ctrl+Shift+P`, `F5`, `Home Shift+End Ctrl+C`. Modifiers are
    /// ctrl/shift/alt/win. Unrecognised keys are refused rather than guessed
    /// at, and a run is capped at 32 keystrokes. `key` carries virtual-key
    /// codes, which name positions on a keyboard rather than characters, so it
    /// deliberately refuses punctuation it cannot place on every layout - use
    /// `type_keys` for that text. A `type_keys` run is capped at 2048 UTF-16
    /// code units, which is fewer than 2048 characters for text outside the
    /// BMP: an emoji costs two.
    pub value: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema, Default)]
pub struct ObserveParams {
    /// "text" (window list + focused elements), "image" (full screen),
    /// or "diff" (only what changed since the last observe).
    pub detail: Option<String>,
    /// Downscale width before encoding. Default 1400; 0 for native.
    pub max_width: Option<u32>,
    /// Render this window on demand instead of reading the desktop.
    ///
    /// Worth trying whenever the target draws with OpenGL or Direct3D - a CAD
    /// or simulation viewport, a game, a video pane. A screen read returns
    /// those regions as flat clear colour, indistinguishable from an empty
    /// viewport, and it has been mistaken for one.
    ///
    /// It is not a guaranteed fix. On at least one OpenGL viewport it was
    /// measured to change nothing: both paths returned identical flat pixels.
    /// If that happens, the content is there and this route cannot read it -
    /// which is a different statement from "the viewport is empty", and the
    /// difference matters.
    pub hwnd: Option<isize>,
}

/// What `launch` did, and what a caller needs to find the result.
#[derive(Debug, Serialize, JsonSchema)]
pub struct LaunchResult {
    /// The name as it was allowlisted and passed to Windows.
    pub launched: String,
    /// Top-level window handles that ALREADY existed when this ran, or `null`
    /// if they could not be enumerated.
    ///
    /// `null` and `[]` are not the same claim and must not be conflated: an
    /// empty list says the desktop had no top-level windows, while `null` says
    /// this call does not know. A caller that diffs against a wrongly-empty
    /// list treats every window it finds as new, which is precisely the
    /// mistake this field exists to prevent.
    ///
    /// `launched` means the process was started - not that it is running, and
    /// not that a window exists. A heavy application can take a minute to put
    /// one up, and some never do.
    ///
    /// Poll `windows` afterwards and take the handle that is NOT in this list.
    /// That is the only reliable way to tell a new instance from one already
    /// open: two instances of the same application share a title and a class
    /// name and differ only by handle, so waiting on the wrong one waits
    /// forever on something that was never going to change.
    pub existing_windows: Option<Vec<isize>>,
    pub detail: String,
}

/// What `wait_for` observed.
#[derive(Debug, Serialize, JsonSchema)]
pub struct WaitResult {
    pub ok: bool,
    /// `ok` | `timeout` | `ambiguous`
    ///
    /// `timeout` means the condition never held and says nothing about why -
    /// the target may be absent, or merely slow, or the window may have been
    /// unwalkable throughout; `detail` distinguishes those. `ambiguous` means
    /// the condition held for more than one element, so there is no single
    /// scope to hand back.
    pub status: String,
    /// The condition that was waited on, echoed back.
    pub until: String,
    /// How many times the window was polled. A count of 1 means the condition
    /// already held and nothing was actually waited for.
    pub polls: u32,
    pub elapsed_ms: f64,
    /// On success for `appears`/`enabled`: the scope and entity to act with,
    /// so a caller need not re-`discover` and race the thing it just waited
    /// for.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub matched: Option<uia::Entity>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub matched_by: Option<crate::text::MatchTier>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct WaitForParams {
    /// Window to watch, from `windows`.
    pub hwnd: isize,
    /// What to wait for. Matched exactly as `act` matches, so a control you
    /// waited for is a control `act` can then find.
    pub select: uia::Selector,
    /// `appears` (default) | `disappears` | `enabled`
    ///
    /// `disappears` waits out a progress dialog or a modal; `enabled` waits
    /// for a button that exists but is greyed.
    pub until: Option<String>,
    /// Give up after this long. Default 10000, capped at 120000 - a wait that
    /// outlives the caller's patience is a hang, not a wait.
    pub timeout_ms: Option<u64>,
    /// Gap between polls. Default 250, floored at 50. Each poll is a full
    /// window walk, so a tight interval on a slow app costs more than it buys.
    pub poll_ms: Option<u64>,
}

#[derive(Debug, Deserialize, JsonSchema, Default)]
pub struct FindTextParams {
    /// Substring to look for. Matching folds case, Unicode composition
    /// (NFD vs NFC) and full-width forms, then falls back to characters OCR
    /// commonly confuses. Omit to return every line on screen.
    pub query: Option<String>,
    /// Cap on returned matches. Default 50.
    pub max_matches: Option<usize>,
    /// Restrict OCR to one window from `windows`. Strongly preferred: fewer
    /// pixels means more magnification and better accuracy, and it stops a
    /// query matching text elsewhere on the desktop.
    pub hwnd: Option<isize>,
    /// BCP-47 recognizer language, e.g. `ja` or `de-DE`. Defaults to the user
    /// profile's languages, which is wrong whenever the profile and the
    /// application disagree - and wrong quietly, since a mismatched recognizer
    /// returns confident nonsense rather than an error. Every response lists
    /// `available_languages`; pick from those.
    pub lang: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct LaunchParams {
    /// Must appear in the launch allowlist.
    pub name: String,
}

#[tool_router]
impl Wincrust {
    /// The allowlist is loaded once by the caller and shared. Loading it here
    /// would re-read the file and repeat a security-relevant syscall
    /// (`SetSecurityInfo`) on every new HTTP session.
    pub fn new(engine: uia::Engine, allowlist: std::sync::Arc<Vec<String>>) -> Self {
        Self { engine, allowlist }
    }

    #[tool(
        name = "windows",
        description = "List top-level windows with their handles, pids and bounds, including \
                       dialogs owned by another window. An owned dialog carries `owned_by` with \
                       its owner's handle, which is what attributes a bare 'Save changes?' prompt \
                       to the application it is blocking - and a blocked application is the usual \
                       reason one is on screen. If something still looks hung with nothing here \
                       to explain it, `observe` with no `hwnd` reads the whole desktop and \
                       `discover` with no `hwnd` reads the foreground window, which is the modal \
                       when there is one."
    )]
    async fn windows(&self) -> Result<Json<serde_json::Value>, McpError> {
        let w = self
            .engine
            .list_windows()
            .await
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;
        Ok(Json(serde_json::json!({ "windows": w })))
    }

    #[tool(
        name = "discover",
        description = "List the actionable elements of a window. Returns a signed `scope` plus \
                       entities each carrying a `path`; pass both to `act`. Always discover before \
                       acting. Prefer `select` over `path` when acting on the result: a path is an \
                       index into the tree as it was, and a tooltip appearing is enough to shift \
                       every index by one, while a selector is re-resolved against the tree as it \
                       is. WITHOUT `hwnd` this reads the foreground window - which is the dialog \
                       when one is blocking an application, so it is the fastest way to see what \
                       something is waiting on."
    )]
    async fn discover(
        &self,
        Parameters(p): Parameters<DiscoverParams>,
    ) -> Result<Json<uia::Discovery>, McpError> {
        let filter = match p.filter.as_deref().unwrap_or("actionable") {
            "all" => uia::Filter::All,
            _ => uia::Filter::Actionable,
        };
        self.engine
            .discover(uia::DiscoverArgs {
                hwnd: p.hwnd,
                max_depth: 24,
                max_elements: p.max_elements.unwrap_or(400),
                ttl_secs: crate::lease::DEFAULT_TTL_SECS,
                filter,
                verbose: false,
                menu_depth: p.menu_depth.unwrap_or(DEFAULT_MENU_DEPTH),
            })
            .await
            .map(Json)
            .map_err(|e| McpError::internal_error(e.to_string(), None))
    }

    #[tool(
        name = "act",
        description = "Act on one element via its UIA control pattern. Verifies the window is \
                       unchanged and the element is still present and enabled before doing anything; \
                       a non-ok `status` means nothing was done and you should discover again."
    )]
    async fn act(
        &self,
        Parameters(p): Parameters<ActParams>,
    ) -> Result<Json<uia::ActResult>, McpError> {
        if guard::engaged() {
            return Err(McpError::internal_error(guard::refusal(), None));
        }
        let select = p.select.filter(|s| !s.is_empty());
        if cfg!(target_os = "macos") && p.allow_ocr {
            return Err(McpError::invalid_params("Automatic OCR clicks are not yet supported on macOS. Use find_text to survey text and act on discovered accessibility elements.", None));
        }
        let ocr_query = p
            .allow_ocr
            .then(|| select.as_ref().and_then(|s| s.name.clone()))
            .flatten();

        let r = self
            .engine
            .act(uia::ActArgs {
                scope: p.scope,
                path: p.path,
                select,
                action: p.action.clone(),
                value: p.value,
            })
            .await
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;

        // Only `not_found` falls through. `ambiguous` means the tree DID have
        // matches and the caller must narrow; `identity_changed` means the world
        // moved and a fresh discover is the right answer. Retrying either with
        // OCR would paper over a condition the caller needs to see.
        match ocr_query {
            Some(q) if r.status == "not_found" => Ok(Json(ocr_fallback(&q, &p.action, r).await)),
            _ => Ok(Json(r)),
        }
    }

    #[tool(
        name = "observe",
        description = "See the screen. `diff` is much cheaper than `image` during a wait - it \
                       returns nothing at all when the screen has not changed. WITHOUT `hwnd` this \
                       reads the desktop, which does NOT contain hardware-accelerated content: an \
                       OpenGL or Direct3D viewport comes back as flat colour that looks exactly \
                       like an empty one. `hwnd` renders that window on demand instead, which \
                       helps SOME applications and is measured as changing nothing on at least \
                       one OpenGL viewport - try it, do not rely on it. If both come back flat, \
                       the content is real and unreadable by this route: do not report the \
                       viewport as empty. A desktop read is also the second opinion when an \
                       application appears hung: it shows a blocking dialog whatever the window \
                       list says, and an app waiting on a prompt has been reported as hung on \
                       the strength of a windowed read that could not show one."
    )]
    async fn observe(
        &self,
        Parameters(p): Parameters<ObserveParams>,
    ) -> Result<CallToolResult, McpError> {
        let detail = p.detail.unwrap_or_else(|| "image".into());
        if detail == "text" {
            let wins = self
                .engine
                .list_windows()
                .await
                .map_err(|e| McpError::internal_error(e.to_string(), None))?;
            let focused = self
                .engine
                .discover(uia::DiscoverArgs {
                    hwnd: None,
                    max_depth: 24,
                    max_elements: 400,
                    ttl_secs: crate::lease::DEFAULT_TTL_SECS,
                    filter: uia::Filter::Actionable,
                    verbose: false,
                    menu_depth: 0,
                })
                .await
                .ok();
            let v = serde_json::json!({
                "process_dpi_awareness": crate::dpi::awareness(),
                "displays": crate::dpi::displays().unwrap_or_default(),
                "windows": wins,
                "focused": focused,
            });
            return Ok(CallToolResult::success(vec![ContentBlock::text(
                v.to_string(),
            )]));
        }

        let is_diff = detail == "diff";
        let max_width = p.max_width.unwrap_or(1400);
        let hwnd = p.hwnd;
        let (obs, png) =
            tokio::task::spawn_blocking(move || capture::observe_bytes(is_diff, max_width, hwnd))
                .await
                .map_err(|e| McpError::internal_error(e.to_string(), None))?
                .map_err(|e| McpError::internal_error(e.to_string(), None))?;

        let meta = serde_json::to_string(&obs).unwrap_or_default();
        let mut out = vec![ContentBlock::text(meta)];
        // No bytes when nothing changed, or when the region hull was judged
        // unrepresentative - the metadata already says what happened.
        if !png.is_empty() {
            use base64::{engine::general_purpose::STANDARD as B64S, Engine};
            out.push(ContentBlock::image(
                B64S.encode(&png),
                "image/png".to_string(),
            ));
        }
        Ok(CallToolResult::success(out))
    }

    #[tool(
        name = "wait_for",
        description = "Block until a control appears, disappears or becomes enabled. Use this \
                       instead of acting immediately after something that takes time - opening a \
                       dialog, loading a file, starting a job. Polling `discover` from outside \
                       costs a round trip per attempt; this polls in-process and hands back a \
                       scope you can act on straight away."
    )]
    async fn wait_for(
        &self,
        Parameters(p): Parameters<WaitForParams>,
    ) -> Result<Json<WaitResult>, McpError> {
        let t0 = std::time::Instant::now();
        let until = p.until.as_deref().unwrap_or("appears").to_ascii_lowercase();
        if !matches!(until.as_str(), "appears" | "disappears" | "enabled") {
            return Err(McpError::invalid_params(
                format!("unknown `until` value {until:?} (appears|disappears|enabled)"),
                None,
            ));
        }
        if p.select.is_empty() {
            return Err(McpError::invalid_params(
                "`select` is empty - it would match every element and return instantly".to_string(),
                None,
            ));
        }
        // Capped, because a wait that outlives the caller is a hang rather than
        // a wait. Floored, because each poll is a full window walk.
        let timeout = std::time::Duration::from_millis(p.timeout_ms.unwrap_or(10_000).min(120_000));
        let poll = std::time::Duration::from_millis(p.poll_ms.unwrap_or(250).max(50));

        let mut polls: u32 = 0;
        // Kept so a timeout can say whether the polls were failing or merely
        // not matching - two very different things to be told after 30 s.
        let mut last_err: Option<String> = None;
        loop {
            polls += 1;
            let elapsed = || t0.elapsed().as_secs_f64() * 1000.0;

            let outcome = self
                .engine
                .discover(uia::DiscoverArgs {
                    hwnd: Some(p.hwnd),
                    max_depth: 24,
                    max_elements: 400,
                    ttl_secs: crate::lease::DEFAULT_TTL_SECS,
                    filter: uia::Filter::All,
                    verbose: false,
                    menu_depth: 0,
                })
                .await;

            match outcome {
                Ok(d) => {
                    // A poll succeeded, so an earlier transient failure is no
                    // longer the story - clearing it keeps the timeout message
                    // from blaming a walk that has since recovered.
                    if last_err.is_some() {
                        last_err = None;
                    }
                    let mut hits: Vec<(uia::Entity, crate::text::MatchTier)> = d
                        .entities
                        .iter()
                        .filter_map(|e| uia::entity_matches(e, &p.select).map(|t| (e.clone(), t)))
                        .collect();
                    crate::text::keep_best(&mut hits, |h| h.1);

                    // `act` refuses when a selector matches more than one
                    // element, because quietly taking the first is how
                    // automation clicks the wrong thing. A `wait_for` that
                    // hands back one of several would launder that same guess
                    // into the very scope the caller then acts on.
                    let candidates: Vec<&(uia::Entity, crate::text::MatchTier)> =
                        match until.as_str() {
                            "appears" => hits.iter().collect(),
                            "enabled" => hits.iter().filter(|(e, _)| e.enabled).collect(),
                            _ => Vec::new(),
                        };
                    if candidates.len() > 1 {
                        let names: Vec<&str> = candidates
                            .iter()
                            .take(4)
                            .map(|(e, _)| e.name.as_str())
                            .collect();
                        return Ok(Json(WaitResult {
                            ok: false,
                            status: "ambiguous".into(),
                            until,
                            polls,
                            elapsed_ms: elapsed(),
                            scope: None,
                            matched: None,
                            matched_by: None,
                            detail: Some(format!(
                                "{} elements satisfy {} ({}). Narrow with automation_id or \
                                 control_type - returning one of them would hand you a scope \
                                 pointing at an arbitrary choice.",
                                candidates.len(),
                                p.select.describe(),
                                names.join(", ")
                            )),
                        }));
                    }
                    let hit = candidates.first().map(|(e, t)| (e.clone(), *t));
                    if until == "disappears" && hits.is_empty() {
                        return Ok(Json(WaitResult {
                            ok: true,
                            status: "ok".into(),
                            until,
                            polls,
                            elapsed_ms: elapsed(),
                            scope: None,
                            matched: None,
                            matched_by: None,
                            detail: None,
                        }));
                    }
                    if let Some((e, tier)) = hit {
                        return Ok(Json(WaitResult {
                            ok: true,
                            status: "ok".into(),
                            until,
                            polls,
                            elapsed_ms: elapsed(),
                            scope: Some(d.scope),
                            matched: Some(e),
                            matched_by: Some(tier),
                            detail: None,
                        }));
                    }
                }
                Err(e) => {
                    // A window that vanishes mid-wait is a legitimate way for
                    // `disappears` to be satisfied.
                    if until == "disappears" {
                        return Ok(Json(WaitResult {
                            ok: true,
                            status: "ok".into(),
                            until,
                            polls,
                            elapsed_ms: elapsed(),
                            scope: None,
                            matched: None,
                            matched_by: None,
                            detail: Some(format!(
                                "the window is no longer reachable, which satisfies `disappears`: {e}"
                            )),
                        }));
                    }
                    // Everywhere else this is a RETRY, not a failure. A window
                    // being walked while it opens, loads or swaps a dialog is
                    // exactly when a walk fails transiently - and exactly when
                    // someone is waiting on it. Giving up on the first error
                    // would abandon the wait at its most likely moment.
                    last_err = Some(e.to_string());
                }
            }

            if t0.elapsed() >= timeout {
                return Ok(Json(WaitResult {
                    ok: false,
                    status: "timeout".into(),
                    until: until.clone(),
                    polls,
                    elapsed_ms: elapsed(),
                    scope: None,
                    matched: None,
                    matched_by: None,
                    detail: Some(match &last_err {
                        Some(e) => format!(
                            "nothing satisfied `{until}` for {} within {} ms; the last poll also \
                             failed to walk the window: {e}",
                            p.select.describe(),
                            timeout.as_millis()
                        ),
                        None => format!(
                            "nothing satisfied `{until}` for {} within {} ms",
                            p.select.describe(),
                            timeout.as_millis()
                        ),
                    }),
                }));
            }
            tokio::time::sleep(poll).await;
        }
    }

    #[tool(
        name = "find_text",
        description = "Read text off the screen with OCR and return where it is. Use this ONLY when \
                       `discover` comes back empty or useless - an app that draws its own interface \
                       exposes no UI tree. Slower and less certain than `discover`; prefer that whenever \
                       it returns anything."
    )]
    async fn find_text(
        &self,
        Parameters(p): Parameters<FindTextParams>,
    ) -> Result<Json<ocr::TextResult>, McpError> {
        let q = p.query;
        let max = p.max_matches.unwrap_or(50);
        let hwnd = p.hwnd;
        let lang = p.lang;
        tokio::task::spawn_blocking(move || {
            ocr::find_text(ocr::FindArgs {
                query: q.as_deref(),
                max_matches: max,
                hwnd,
                scale: 0.0,
                image: None,
                prep: crate::capture::Prep::None,
                lang: lang.as_deref(),
            })
        })
        .await
        .map_err(|e| McpError::internal_error(e.to_string(), None))?
        .map(Json)
        .map_err(|e| McpError::internal_error(e.to_string(), None))
    }

    #[tool(
        name = "launch",
        description = "Start an application. Only names present in the server's launch allowlist \
                       are permitted; there is no way to run an arbitrary command. The name must \
                       ALSO be one Windows can resolve - on PATH, or registered under App Paths - \
                       otherwise give the full path to the .exe. Being allowlisted and being \
                       resolvable are two separate things, and passing the first says nothing \
                       about the second. Returns the windows that existed beforehand: `launched` \
                       means started, NOT running, so poll `windows` afterwards and take the \
                       handle that is not among them."
    )]
    async fn launch(
        &self,
        Parameters(p): Parameters<LaunchParams>,
    ) -> Result<Json<LaunchResult>, McpError> {
        if guard::engaged() {
            return Err(McpError::internal_error(guard::refusal(), None));
        }
        if cfg!(target_os = "macos") {
            return Err(McpError::invalid_params("Application launch is not yet supported by the macOS backend; open the application first.", None));
        }
        let want = p.name.trim().to_lowercase();
        if !self.allowlist.contains(&want) {
            return Err(McpError::invalid_params(
                format!(
                    "'{}' is not in the launch allowlist ({} entries). Add it to \
                     %LOCALAPPDATA%\\wincrust\\launch-allowlist.txt on the host - and use the \
                     full path to the .exe unless the name is on PATH or registered under App \
                     Paths, since an allowlisted name Windows cannot resolve will still fail.",
                    p.name,
                    self.allowlist.len()
                ),
                None,
            ));
        }
        // Snapshot before starting. `cmd /C start` returns as soon as it has
        // handed off, so the pid it yields is the shell's and tells a caller
        // nothing; what a caller actually needs is a way to recognise the
        // window that appears later. Two instances of one application are
        // identical in title and class, so "not in this list" is the only
        // thing that distinguishes them.
        let snapshot = self.engine.list_windows().await;
        let snapshot_err = snapshot.as_ref().err().map(|e| e.to_string());
        let existing_windows: Option<Vec<isize>> = snapshot
            .ok()
            .map(|ws| ws.into_iter().map(|w| w.hwnd).collect());

        std::process::Command::new("cmd")
            .args(["/C", "start", "", &p.name])
            .spawn()
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;

        let detail = match (&existing_windows, &snapshot_err) {
            (Some(w), _) => format!(
                "started - which is not the same as running. {} top-level window(s) existed \
                 beforehand and are listed in `existing_windows`; poll `windows` and take the \
                 handle that is not among them. A heavy application can take a minute, and two \
                 instances of one application share a title and a class name, so the handle is \
                 the only thing that tells them apart.",
                w.len()
            ),
            // Reported rather than defaulted to empty: an empty list would
            // claim the desktop was bare and make every window found
            // afterwards look new. The launch still happened, so this reports
            // it and withholds the part it cannot vouch for.
            (None, Some(e)) => format!(
                "started - which is not the same as running. The windows open beforehand could \
                 NOT be enumerated ({e}), so `existing_windows` is null rather than empty and \
                 there is no way here to tell a new window from one already open. Call \
                 `windows` and compare against what you knew before."
            ),
            (None, None) => "started - which is not the same as running.".to_string(),
        };
        Ok(Json(LaunchResult {
            launched: p.name.clone(),
            detail,
            existing_windows,
        }))
    }
}

#[tool_handler]
impl ServerHandler for Wincrust {
    fn get_info(&self) -> ServerInfo {
        // ServerInfo and Implementation are #[non_exhaustive]: build from
        // Default and assign, rather than a struct literal.
        let mut me = Implementation::default();
        me.name = "wincrust".into();
        me.version = env!("CARGO_PKG_VERSION").into();

        let mut info = ServerInfo::default();
        info.protocol_version = ProtocolVersion::default();
        info.capabilities = ServerCapabilities::builder().enable_tools().build();
        info.server_info = me;
        // Rules ride the handshake rather than an instructions file, so a
        // client that never reads project docs still inherits them.
        info.instructions = Some(
            "Windows desktop automation.\n\
             - ALWAYS `discover` before `act`. Pass back the response's `scope` together with \
               the entity's `path`; never invent either.\n\
             - A `status` other than \"ok\" means NOTHING happened. `identity_changed` or \
               `not_found` means the window moved on - discover again rather than retrying.\n\
             - Prefer `observe` detail=\"diff\" while waiting for something to happen; it costs \
               nothing when the screen is static. Use detail=\"image\" only when you need to see \
               an app that exposes no UI tree.\n\
             - Scopes expire after 60s.\n\
             - There is no shell here. For files, processes and commands, use SSH instead."
                .into(),
        );
        #[cfg(target_os = "macos")]
        {
            info.instructions = Some(format!(
            "macOS desktop automation. Call windows, then discover before act. hwnd is an opaque process-local window ID, not a Win32 handle. Scopes expire within {} seconds and may be evicted; discover again after every action. Use only returned actions. A successful action means dispatched, not verified: inspect the resulting UI. Errors can occur after focus changes; do not retry blindly. Keys use Command/Control/Option explicitly. Screenshot and OCR coordinates are desktop points with a top-left origin. macOS 14+ is required for screenshots. Menus live on the application, not the window: discover returns the top-level menu names by default, and menu_depth=3 returns the commands inside them, which you can act on directly without opening the menu. Window capture, automatic OCR clicks and launch are not supported in this initial backend. Permissions: {}",
            crate::lease::DEFAULT_TTL_SECS, crate::macos::diagnostics()
        ));
        }
        info
    }
}

/// Bring the server up on stdio (local Claude Code) or streamable HTTP (remote
/// over Tailscale).
pub async fn serve(
    engine: uia::Engine,
    transport: &str,
    host: &str,
    port: u16,
    auth_key: Option<String>,
    ip_allowlist: Vec<String>,
) -> anyhow::Result<()> {
    match transport {
        "stdio" => {
            use rmcp::ServiceExt;
            let allowlist = std::sync::Arc::new(guard::load_allowlist());
            let service = Wincrust::new(engine, allowlist)
                .serve(rmcp::transport::stdio())
                .await?;
            service.waiting().await?;
            Ok(())
        }
        "http" => {
            // Decided on the resolved address, not the spelling: "localhost"
            // can resolve to a non-loopback address on a badly configured host.
            let loopback = tokio::net::lookup_host((host, port))
                .await
                .map(|mut a| a.all(|s| s.ip().is_loopback()))
                .unwrap_or(false);
            // Refusing rather than warning: this process holds an admin token,
            // and an unauthenticated bind is not a mistake worth allowing a flag
            // to override.
            if !loopback && auth_key.is_none() {
                anyhow::bail!(
                    "refusing to bind {host} without --auth-key: this server runs elevated"
                );
            }
            serve_http(engine, host, port, auth_key, ip_allowlist).await
        }
        other => anyhow::bail!("unknown transport '{other}' (stdio|http)"),
    }
}

/// The `Host` header values this server accepts.
///
/// rmcp validates the inbound `Host` against an allowlist defaulting to
/// loopback only, as protection against DNS rebinding: a page in a browser can
/// be tricked into resolving an attacker's name to 127.0.0.1 and talking to a
/// local server, and the `Host` header is what gives that away.
///
/// That default is right for a server that only ever listens on localhost, and
/// wrong for this one, whose entire remote story is binding a Tailscale
/// address. Passing the default through meant every non-loopback bind answered
/// 403 - the transport was unusable for the one case it exists for.
///
/// Clearing the check would trade one bug for a worse one. Instead the address
/// the operator explicitly asked to bind is added, both bare and with the port,
/// since either spelling can arrive in the header. Everything else is still
/// rejected, and an empty default is left empty: rmcp reads that as "checking
/// disabled", and this function must not quietly re-enable it.
fn allowed_hosts_for(mut default: Vec<String>, host: &str, port: u16) -> Vec<String> {
    if default.is_empty() {
        return default;
    }
    // An IPv6 literal has to be bracketed to survive authority parsing:
    // `fd7a::1:8900` is ambiguous with the address itself and does not parse,
    // so an unbracketed entry becomes a dead one that matches nothing. rmcp
    // strips brackets when normalising, so the bracketed form still compares
    // equal to a plain `Host` header.
    let bare = match host.trim_matches(['[', ']']).parse::<std::net::Ipv6Addr>() {
        Ok(_) => format!("[{}]", host.trim_matches(['[', ']'])),
        Err(_) => host.to_string(),
    };
    let with_port = format!("{bare}:{port}");
    for h in [bare, with_port] {
        if !default.contains(&h) {
            default.push(h);
        }
    }
    default
}

async fn serve_http(
    engine: uia::Engine,
    host: &str,
    port: u16,
    auth_key: Option<String>,
    ip_allowlist: Vec<String>,
) -> anyhow::Result<()> {
    use axum::extract::ConnectInfo;
    use axum::http::StatusCode;
    use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
    use rmcp::transport::streamable_http_server::{
        StreamableHttpServerConfig, StreamableHttpService,
    };
    use std::net::SocketAddr;

    let allowlist = std::sync::Arc::new(guard::load_allowlist());

    let mut cfg = StreamableHttpServerConfig::default();
    cfg.allowed_hosts = allowed_hosts_for(cfg.allowed_hosts, host, port);
    let svc = StreamableHttpService::new(
        move || Ok(Wincrust::new(engine.clone(), allowlist.clone())),
        LocalSessionManager::default().into(),
        cfg,
    );

    let key = auth_key.clone();
    let allow_count = ip_allowlist.len();
    let allow = std::sync::Arc::new(ip_allowlist);

    let app = axum::Router::new()
        .nest_service("/mcp", svc)
        .layer(axum::middleware::from_fn(
            move |req: axum::extract::Request, next: axum::middleware::Next| {
                let key = key.clone();
                let allow = allow.clone();
                async move {
                    // Peer address first: an unauthorised source should not even get
                    // to present a token, and a wrong token should not reveal
                    // whether the address would have been accepted.
                    if !allow.is_empty() {
                        let ip = req
                            .extensions()
                            .get::<ConnectInfo<SocketAddr>>()
                            .map(|c| c.0.ip().to_string())
                            .unwrap_or_default();
                        if !allow.contains(&ip) {
                            tracing::warn!("rejected connection from {ip}");
                            return Err(StatusCode::FORBIDDEN);
                        }
                    }
                    if let Some(k) = key.as_ref() {
                        let ok = req
                            .headers()
                            .get(axum::http::header::AUTHORIZATION)
                            .and_then(|v| v.to_str().ok())
                            .and_then(|v| v.strip_prefix("Bearer "))
                            .map(|t| constant_time_eq(t.as_bytes(), k.as_bytes()))
                            .unwrap_or(false);
                        if !ok {
                            return Err(StatusCode::UNAUTHORIZED);
                        }
                    }
                    Ok(next.run(req).await)
                }
            },
        ));

    // `SocketAddr::from_str` neither resolves names nor tolerates an unbracketed
    // IPv6 literal, so the loopback check above accepted "localhost" and "::1"
    // as valid while binding them failed. Resolve instead of parsing.
    let addr: SocketAddr = tokio::net::lookup_host((host, port))
        .await
        .map_err(|e| anyhow::anyhow!("cannot resolve {host}:{port}: {e}"))?
        .next()
        .ok_or_else(|| anyhow::anyhow!("{host}:{port} resolved to no addresses"))?;
    let listener = tokio::net::TcpListener::bind(addr).await?;
    tracing::info!(
        "wincrust listening on http://{addr}/mcp  (auth: {}, ip allowlist entries: {})",
        if auth_key.is_some() { "on" } else { "OFF" },
        allow_count
    );
    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .await?;
    Ok(())
}

/// Comparison that does not leak the correct prefix length through timing.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// Second attempt for an application that exposes no UI tree.
///
/// Kept out of the UIA engine on purpose: this path never touches COM, and
/// folding it into the act state machine would blur the line between "acted on
/// a control" and "clicked a pixel" - a distinction the caller has to be able
/// to see in the result.
async fn ocr_fallback(query: &str, action: &str, mut r: uia::ActResult) -> uia::ActResult {
    let t0 = std::time::Instant::now();
    let q = query.to_string();
    let found = tokio::task::spawn_blocking(move || {
        ocr::find_text(ocr::FindArgs {
            query: Some(&q),
            max_matches: 8,
            hwnd: None,
            scale: 0.0,
            image: None,
            prep: crate::capture::Prep::None,
            // Profile default: the fallback has no idea what language the app
            // it is rescuing renders in, and guessing would be worse.
            lang: None,
        })
    })
    .await;

    let mut fail = |status: &str, detail: String| -> uia::ActResult {
        r.ok = false;
        r.status = status.to_string();
        r.resolved_by = "ocr".into();
        r.matched_by = None;
        r.next_scope = None;
        r.detail = Some(detail);
        r.screen_changed = None;
        r.elapsed_ms += t0.elapsed().as_secs_f64() * 1000.0;
        r.clone()
    };

    let mut res = match found {
        Ok(Ok(res)) => res,
        Ok(Err(e)) => {
            return fail(
                "not_found",
                format!("not in the UI tree, and OCR failed: {e}"),
            )
        }
        Err(e) => return fail("not_found", format!("OCR task failed: {e}")),
    };
    // This path needs exactly one target, so a clean read outranks one that
    // survived only by folding characters the recogniser confuses. Narrowing
    // here rescues clicks that would otherwise be refused as `ambiguous`;
    // `find_text` itself stays unfiltered, being a survey rather than a
    // resolution.
    crate::text::keep_best(&mut res.matches, |m| m.matched_by);

    match res.matches.len() {
        0 => {
            return fail(
                "not_found",
                format!(
                    "{query:?} is neither in the UI tree nor on screen ({} lines read)",
                    res.lines_seen
                ),
            )
        }
        1 => {}
        n => {
            return fail(
                "ambiguous",
                format!(
                    "{query:?} appears {n} times on screen. OCR cannot disambiguate by role - \
                     narrow the query or use a UI-tree selector."
                ),
            )
        }
    }
    if action != "click" {
        return fail(
            "pattern_gone",
            format!(
                "OCR found {query:?} but only \"click\" is possible on a screen-read target - \
                     there is no control pattern to perform {action:?}"
            ),
        );
    }

    let m = &res.matches[0];
    let (x, y) = m.click_at;
    // Re-checked immediately before the input, not only at entry: OCR takes
    // hundreds of milliseconds, which is long enough for someone to reach for
    // the corner precisely because they want this to stop.
    if guard::engaged() {
        return fail("stopped", guard::refusal());
    }
    // Baseline taken as late as possible, so the comparison afterwards reflects
    // the click and not whatever the screen was doing during OCR.
    let before = tokio::task::spawn_blocking(crate::capture::grab)
        .await
        .ok()
        .and_then(|r| r.ok());

    if let Err(e) = crate::input::click_at(x, y) {
        return fail(
            "error",
            format!("OCR found {query:?} at ({x},{y}) but the click failed: {e}"),
        );
    }

    // The UIA path gets perception guards - window identity, bounds, enabled
    // state. A coordinate click gets none of that, so the nearest available
    // evidence is what the screen did next. Long enough for a repaint, short
    // enough not to catch an unrelated animation.
    tokio::time::sleep(std::time::Duration::from_millis(SETTLE_MS)).await;
    let changed = match before {
        Some(b) => tokio::task::spawn_blocking(crate::capture::grab)
            .await
            .ok()
            .and_then(|r| r.ok())
            .and_then(|a| crate::capture::compare_frames(&b, &a)),
        None => None,
    };

    // A loose read is worth saying out loud: the caller asked for one string
    // and OCR clicked a different one it judged equivalent.
    let loose = match m.matched_by {
        crate::text::MatchTier::Exact | crate::text::MatchTier::Case => String::new(),
        t => format!(
            " The text was matched at the {} tier rather than read back verbatim,",
            t.as_str()
        ),
    };

    let note = match &changed {
        Some(c) => format!(
            "the screen then changed over {:.2}% of its area at ({},{}) {}x{}",
            c.fraction * 100.0,
            c.x,
            c.y,
            c.w,
            c.h
        ),
        None => "the screen did NOT change afterwards - the click most likely missed, though a \
                 control already in the requested state would look the same"
            .to_string(),
    };

    uia::ActResult {
        ok: true,
        action: action.to_string(),
        status: "ok".into(),
        target: m.text.clone(),
        resolved_by: "ocr".into(),
        matched_by: Some(m.matched_by),
        // The OCR path never had a scope to refresh - it resolved by pixels.
        next_scope: None,
        detail: Some(format!(
            "not in the UI tree; clicked the screen at ({x},{y}) where OCR read {:?}. \
             No control pattern was involved, so this reports that the click was sent, \
             not that the application handled it -{loose} {note}.",
            m.text
        )),
        screen_changed: changed,
        elapsed_ms: r.elapsed_ms + t0.elapsed().as_secs_f64() * 1000.0,
    }
}

/// How long to wait between a coordinate click and the confirming capture.
const SETTLE_MS: u64 = 250;

#[cfg(test)]
mod host_tests {
    use super::allowed_hosts_for;

    fn loopback() -> Vec<String> {
        vec!["localhost".into(), "127.0.0.1".into(), "::1".into()]
    }

    /// rmcp's own matching, reproduced through the same `http::uri::Authority`
    /// it uses: parse each entry, fall back to a bare hostname when it will not
    /// parse, strip brackets, and compare. Asserting on the returned `Vec`
    /// alone proved only that a string was present - not that rmcp would ever
    /// match it, which is precisely where the IPv6 entry went wrong.
    fn rmcp_would_allow(allowed: &[String], host_header: &str) -> bool {
        fn norm(h: &str) -> String {
            h.trim_matches('[').trim_matches(']').to_ascii_lowercase()
        }
        fn parse(s: &str) -> (String, Option<u16>) {
            match http::uri::Authority::try_from(s) {
                Ok(a) => (norm(a.host()), a.port_u16()),
                Err(_) => (norm(s), None),
            }
        }
        let (hh, hp) = parse(host_header);
        allowed
            .iter()
            .map(|a| parse(a))
            .any(|(ah, ap)| ah == hh && ap.is_none_or(|p| hp == Some(p)))
    }

    #[test]
    fn a_tailscale_bind_becomes_reachable() {
        // The shipped 0.1.0 bug: this address answered 403 on every request.
        let out = allowed_hosts_for(loopback(), "100.78.123.110", 8900);
        assert!(rmcp_would_allow(&out, "100.78.123.110:8900"));
    }

    #[test]
    fn an_ipv6_bind_is_bracketed_so_the_port_entry_is_not_dead() {
        // `fd7a::1:8900` does not parse as an authority, so an unbracketed
        // entry silently matches nothing and the port is never enforced.
        let out = allowed_hosts_for(loopback(), "fd7a:115c:a1e0::1", 8900);
        assert!(
            out.contains(&"[fd7a:115c:a1e0::1]:8900".to_string()),
            "{out:?}"
        );
        assert!(rmcp_would_allow(&out, "[fd7a:115c:a1e0::1]:8900"));
    }

    #[test]
    fn an_already_bracketed_ipv6_host_is_not_double_bracketed() {
        let out = allowed_hosts_for(loopback(), "[fd7a:115c:a1e0::1]", 8900);
        assert!(!out.iter().any(|h| h.contains("[[")), "{out:?}");
        assert!(rmcp_would_allow(&out, "[fd7a:115c:a1e0::1]:8900"));
    }

    #[test]
    fn loopback_still_works_and_is_not_duplicated() {
        let out = allowed_hosts_for(loopback(), "127.0.0.1", 8900);
        assert_eq!(out.iter().filter(|h| *h == "127.0.0.1").count(), 1);
        assert!(rmcp_would_allow(&out, "127.0.0.1:8900"));
    }

    #[test]
    fn an_unlisted_host_is_rejected_by_the_same_matching() {
        // Named for what it checks: rmcp's matching, not just Vec contents.
        let out = allowed_hosts_for(loopback(), "100.78.123.110", 8900);
        assert!(!rmcp_would_allow(&out, "evil.example.com"));
        assert!(!rmcp_would_allow(&out, "evil.example.com:8900"));
        assert!(!rmcp_would_allow(&out, "100.78.123.111:8900"));
    }

    #[test]
    fn an_empty_default_stays_empty() {
        // rmcp reads an empty list as "host checking disabled". If an operator
        // or a future rmcp default turns it off, this must not switch it back
        // on with a one-entry allowlist that rejects everything else.
        assert!(allowed_hosts_for(Vec::new(), "100.78.123.110", 8900).is_empty());
    }
}
