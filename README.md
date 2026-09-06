# wincrust 🦀

**Windows and macOS desktop automation over MCP — pure Rust, one binary (7 MB Windows, 5 MB macOS), seven tools, no shell.**

[![License: MIT OR Apache-2.0](https://img.shields.io/crates/l/wincrust.svg)](#licence)
[![crates.io](https://img.shields.io/crates/v/wincrust.svg)](https://crates.io/crates/wincrust)
[![rust](https://img.shields.io/badge/rust-1.96%2B-orange.svg)](https://www.rust-lang.org/)
[![ci](https://github.com/nhatvu148/wincrust/actions/workflows/ci.yml/badge.svg)](https://github.com/nhatvu148/wincrust/actions/workflows/ci.yml)

Let an agent see and drive a Windows desktop: enumerate windows, read a window's
UI Automation tree, click and type through control patterns, wait for a control
to appear, and read the screen with OCR when an application has no tree at all.
It runs elevated, so it reaches windows a normal process cannot.

**win** for the platform, **crust** for the language — it contains *rust*, and
crustacean is where *Rustacean* comes from.

```bash
cargo install wincrust
```

Driving it from another machine takes two more steps — see
[below](#driving-it-from-another-machine). Already running? Start with
[docs/USING.md](docs/USING.md). On a Mac, start with [macOS](#macos) — the same
tools run there over Accessibility, with three of them reported unsupported and
`wincrust doctor` to tell you which.

## Why

The obvious alternative, [Windows-MCP](https://github.com/CursorTouch/Windows-MCP), works well and is
worth using. wincrust exists for one reason: **integrity level**.

To reach UAC-adjacent dialogs, an automation server must run at High integrity. Windows-MCP at
High integrity means **95 Python packages and 105 native modules** loaded into a process holding an
admin token on a network socket, with tools including `PowerShell` and `Registry`.

wincrust has 16 direct dependencies and 168 transitive crates. That is not a small number, and
it would be dishonest to pretend otherwise — the difference is in kind, not count: Rust dependencies
are resolved at compile time and dead-code-eliminated, with no interpreter, no dynamic import and no
`eval` in the running process. And the tool surface is seven, none of which is a shell.

Elevation itself is not the reason — that is a `-RunLevel Highest` flag on a scheduled task, no code
required. The reason is what elevation does to the cost of a large dependency tree.

## Tools

| tool | |
|---|---|
| `windows` | top-level windows with handles, pids, bounds |
| `discover` | actionable elements + a signed scope |
| `act` | click / type / toggle / expand / select, via UIA control patterns |
| `act` + `key` | send keystrokes to a resolved control — `Enter`, `Ctrl+S`, `F5` |
| `observe` | `text` \| `image` \| `diff` |
| `wait_for` | block until a control appears, disappears or becomes enabled |
| `find_text` | OCR the screen, return text with coordinates — for apps with no UI tree |
| `act` + `allow_ocr` | when the UI tree has no match, click what OCR read instead (opt-in) |
| `launch` | allowlist-only; fails closed |

No shell, no registry, no filesystem, no arbitrary process spawn. Use SSH for those — it does not
run with an admin token.

## Driving it from another machine

Once it is running, [docs/USING.md](docs/USING.md) covers what to actually ask
it, how to read what `act` gives back, and - more useful than either - when to
use SSH instead. [skills/wincrust](skills/wincrust/SKILL.md) is the same
judgement as a Claude Code skill: `cp -r skills/wincrust ~/.claude/skills/`.

The server has to run on Windows, because it drives a Windows desktop. The
client does not: this is the arrangement it was built for, a Mac or Linux
machine running the agent and a Windows box running the desktop.

Two things have to be true on the Windows side, and neither is obvious.

Run the setup from an **elevated** PowerShell. Registering a task with
`-RunLevel Highest` needs an admin token, and that flag is what clears UIPI -
without it the server cannot act on elevated windows, which is half the point.
Day-to-day status, logs, start and stop need no elevation.

There is a script in the repo that does all of it -
[`scripts/wincrust-serve.ps1`](scripts/wincrust-serve.ps1), or `task setup
CLIENT_IP=<your-client-tailscale-ip>` if you use [Task](https://taskfile.dev).
It is not shipped in the published crate, which stays a single binary and
nothing else. What follows is what the script does, because the two things it
gets right are the two things that are easy to get wrong by hand.

**It must run in the interactive session.** A process started over SSH lands in
session 0, which has no desktop: `windows` returns an empty list, capture
returns a picture of nothing, and none of it looks like an error. Task
Scheduler is what crosses that boundary, and the same registration buys
elevation:

```powershell
cargo install wincrust
$key = [Convert]::ToBase64String((1..32 | ForEach-Object { Get-Random -Max 256 }))
New-Item -ItemType Directory -Force "$env:LOCALAPPDATA\wincrust" | Out-Null
Set-Content "$env:LOCALAPPDATA\wincrust\auth-key.txt" $key -NoNewline

# Read the key from the file at launch, so it never appears in the task
# definition or in any process command line.
@"
@echo off
set /p WINCRUST_AUTH_KEY=<""%LOCALAPPDATA%\wincrust\auth-key.txt""
""%USERPROFILE%\.cargo\bin\wincrust.exe"" serve --transport http ^
  --host <this-machine-tailscale-ip> --port 8900 ^
  --ip-allowlist <your-client-tailscale-ip>
"@ | Set-Content "$env:LOCALAPPDATA\wincrust\serve.cmd" -Encoding ASCII

$a = New-ScheduledTaskAction -Execute "cmd.exe" -Argument "/c `"$env:LOCALAPPDATA\wincrust\serve.cmd`""
$p = New-ScheduledTaskPrincipal -UserId $env:USERNAME -LogonType Interactive -RunLevel Highest
$s = New-ScheduledTaskSettingsSet -AllowStartIfOnBatteries -DontStopIfGoingOnBatteries -ExecutionTimeLimit ([TimeSpan]::Zero)
Register-ScheduledTask -TaskName wincrust-serve -Action $a -Principal $p -Settings $s -Force
Start-ScheduledTask -TaskName wincrust-serve
```

**`--ip-allowlist` is not optional in spirit.** A bearer token is one secret
away from an elevated desktop, and a tailnet often contains machines belonging
to other people. Name the client addresses; everything else is refused before
it can present a token.

Register it with `powershell.exe -WindowStyle Hidden`, not `cmd.exe /c`. An
interactive task with a console action puts a visible window on the desktop,
and a visible window gets closed - which surfaces later as a server that died
with exit code `0xC000013A`, which is Ctrl+C.

Then on the client:

```json
{
  "mcpServers": {
    "wincrust": {
      "type": "http",
      "url": "http://<windows-tailscale-ip>:8900/mcp",
      "headers": { "Authorization": "Bearer <the key from auth-key.txt>" }
    }
  }
}
```

Three refusals are worth recognising, because they look alike from the client:

| | meaning |
|---|---|
| `401` | the token is wrong or absent |
| `403` | the source address is not in `--ip-allowlist`, **or** the `Host` header is not the address the server was told to bind |
| empty window list, no error | the server is running in session 0 - it is not on a desktop |

## Where to look next

| | |
|---|---|
| [docs/USING.md](docs/USING.md) | what to ask it, how to read a result, and **when to use SSH instead** |
| [skills/wincrust](skills/wincrust/SKILL.md) | the same judgement as a Claude Code skill — `cp -r skills/wincrust ~/.claude/skills/` |
| [scripts/wincrust-serve.ps1](scripts/wincrust-serve.ps1) | install, key, scheduled task, start — `task setup CLIENT_IP=<ip>` |
| [What is actually verified](#what-is-actually-verified) | measured on hardware, and what is still untested |

## Design

**One COM thread.** `IUIAutomation` and its elements are neither `Send` nor `Sync`, so they cannot
cross an await or migrate between tokio workers. All of UIA is pinned to one MTA thread for the
process lifetime; async callers reach it over `mpsc` + `oneshot`.

**One cross-process call per discovery.** `SetTreeScope(Subtree)` on the cache request means
`BuildUpdatedCache` marshals the whole subtree in a single hop; the `Cached*` getters are then local
reads. The uncached getters cost one round trip *per property per element*.

**Scopes, not coordinates.** `discover` returns one signed scope (window + generation + expiry) and
per-entity child-index paths. `act` re-finds the element and verifies the window is unchanged, the
path resolves, and the control is enabled — before doing anything. A non-`ok` status means nothing
happened.

**Patterns, not synthetic input.** `Invoke()` reaches a control without stealing focus, moving the
cursor, or requiring the window to be on top.

**Stateless.** The scope is a signed token, not a session key - there is no lease table to look up,
expire or garbage-collect. The signing key is generated per process and never written to disk, so a
restart does rotate it; scopes live 60 seconds, which makes that a non-event.

## Measured

Same window, same moment, against Windows-MCP:

| | Windows-MCP | wincrust |
|---|---|---|
| VS Code, 175 entities | 449 ms | **36 ms** |
| Task Manager, 174 entities | 5,477 ms | **665 ms** |
| deploy | 402 MB, 7,734 files | **6.4 MB, 1 file** |

Latency is app-dependent; Task Manager is slow for both.

## Safety

- **Emergency stop** — park the physical mouse within 10 px of the origin for 500 ms and all input is
  refused, latched for 30 s after you move away. `discover` and `observe` keep working: halting an
  agent should stop it touching things, not blind it.
- **Auth** — bearer token, constant-time compare, optional IP allowlist. Refuses to bind a
  non-loopback address without a key.
- **`launch`** — allowlist only. A missing allowlist permits nothing, and so does one that cannot be
  stamped with a High mandatory label: an allowlist a Medium-integrity process could append to is a
  privilege-escalation path, not a config inconvenience.

## What is actually verified

Exercised by hand on **two machines**: Windows 11 Home 25H2 (build 26200) on
one and two 1920x1080 displays, and Windows Server 2022 Datacenter (build
20348) in an RDP session. Both x86_64, en-US.

| | status |
|---|---|
| `discover` / `act` / `observe` | verified against VS Code, Task Manager and an elevated shell |
| emergency stop, allowlist fail-closed, HTTP auth | verified end to end |
| lease signing, diff heuristics, stop state machine, label matching, coordinate arithmetic | unit tested (51 tests) |
| **display scaling** | verified at **100%, 125%, 150% and 175%** — see below |
| **multiple monitors** | **verified** on two displays, including a negative virtual-screen origin — see below |
| **non-English labels** | **verified** against a real UIA tree carrying Japanese, German, Vietnamese and full-width labels — see below |
| ARM64 | cross-compiles in CI (`aarch64-pc-windows-msvc`); never run, and never on an ARM64 host |
| Windows 10 | not tested ([#6](https://github.com/nhatvu148/wincrust/issues/6)) |
| **Windows Server 2022** | **verified** in an RDP session — build 20348, the pre-Windows-11 shell generation |
| Windows Server *without an interactive desktop* | out of scope, and unfixable: this crate requires a desktop |

### A second platform

Windows Server 2022 Datacenter, build 20348, driven through an interactive RDP
session. `windows` enumerated ten top-level windows, `discover` returned
nineteen entities for the taskbar with their real names, `act` resolved a
selector at the `exact` tier, and `find_text` read the screen with the en-US
recognizer. The 51 unit tests and clippy pass there too.

Two things this is worth more than a second green tick for.

Build 20348 is the **Windows 10 21H2 branch** — the pre-Windows-11 shell, with
the old taskbar and window-frame metrics. That is the generation where
Windows 10 differences would actually live, so it narrows [#6](https://github.com/nhatvu148/wincrust/issues/6)
considerably without closing it. Windows 10 itself is still untested.

And it corrected this README, which used to say Windows Server was out of
scope. It is not: what is out of scope is a machine with no interactive
desktop, which some Server configurations have and this one did not. The
distinction is the desktop, not the edition.

Session 0 isolation reproduced there exactly, on a machine that shares nothing
with the development laptop: over SSH the process lands in session 0, sees a
phantom 1024x768 desktop, enumerates **zero** windows, and the capture guard
refuses rather than returning a picture of nothing.

### Multiple monitors

The risk here is not that a second display fails to appear — it is that a click
computed on one monitor lands on another, silently. `SendInput` takes
normalised coordinates over the *virtual* screen, so the arithmetic depends on
an origin that is `(0,0)` in the common layout and negative as soon as a
monitor sits left of or above the primary. That second case is the one worth
proving, and it cannot be reached by plugging a monitor in on the right.

Both were tested by pointing wincrust at text **painted with GDI**, which has
no UI Automation node, so `act` could not resolve it through the tree and had
to use the coordinate path. The harness window recorded the screen point it
actually received, making the check independent of anything wincrust reports
about itself:

| layout | virtual origin | wincrust aimed at | window received | match |
|---|---|---|---|---|
| second display right | (0,0) | (2270, 319) | (2270, 319) | exact |
| second display **left** | **(-1920,0)** | (-1570, 319) | (-1570, 319) | exact |

Both resolved `resolved_by: "ocr"`, confirming the coordinate path was
exercised rather than the control-pattern shortcut, which involves no
coordinates at all and would have proved nothing.

Not covered: displays at **different scale factors**. Scaling is verified at
100/125/150/175% on a single display; mixed-DPI across two monitors is a
distinct case and was not tested. No issue tracks it - nothing seen here
suggests it is broken, and filing one would assert a defect nobody has
observed.

### Localised labels

Name matching is tiered, tightest first — `exact`, `case`, `normalized`,
`affix` — and a search keeps only its best tier, so leniency can never turn a
selector that used to resolve one element into `ambiguous`. Every result
reports which tier it used as `matched_by`.

Verified against a Win32 window built with genuinely localised control labels.
`toggle` asks a control for a pattern it does not have, so each probe resolves
the selector, reports its tier, and changes nothing:

| probe | selector | status | `matched_by` |
|---|---|---|---|
| Japanese menu, verbatim | `ファイル(F)` | `pattern_gone` | `exact` |
| Japanese menu, mnemonic dropped | `ファイル` | `pattern_gone` | `affix` |
| German | `öffnen` vs `Öffnen` | `pattern_gone` | `case` |
| Vietnamese, composed | `Tệp thử` (NFC) | `pattern_gone` | `exact` |
| Vietnamese, decomposed | `Tệp thử` (NFD) | `pattern_gone` | `normalized` |
| full-width label, ASCII query | `Full` vs `Ｆｕｌｌ` | `pattern_gone` | `normalized` |
| disabled control | `Disabled Item` | `disabled` | `exact` |
| **`Save` beside `SAVE`** | `Save` | `pattern_gone` | `exact` |
| absent | — | `not_found` | — |

The NFD row is the one that matters in practice: macOS emits NFD and Windows
reports NFC, so a name typed on a Mac driving a Windows box could never have
compared equal. The `Save`/`SAVE` row is the safety property — it resolves to
**one** element rather than reporting two.

What this does **not** cover is a localised Windows *install*. The labels are
real and so is the UIA tree, but the OS is en-US; a German or Japanese Windows
end to end remains untested ([#6](https://github.com/nhatvu148/wincrust/issues/6)).

OCR carries the same ladder plus a loosest rung for glyphs the recogniser
confuses, and its language is selectable: the engine otherwise follows the
*user profile*, which returns confident nonsense when profile and application
disagree. Every response lists `available_languages`.

### Display scaling

| scale | dpi | window bounds | a titlebar button's `click_at` | `act` |
|---|---|---|---|---|
| 100% | 96 | 1920x1080 | — | ok |
| 125% | 120 | 1936x1036 | (1777, 21) | ok, window minimised |
| 150% | 144 | 1936x1024 | (1749, 25) | ok, window minimised |
| 175% | 168 | 1936x1012 | (1722, 30) | ok, window minimised |

Two things make this more than four passing checks. The button moves left and
down monotonically as scale rises, because titlebar controls grow with it -
which is what physical-pixel reporting should produce, where a virtualised build
would report coordinates shrinking toward the origin. And 175% is the case that
could have failed: 1920 / 1.75 = 1097.14 does not divide cleanly, so rounding
artifacts would surface there.

`wincrust displays` prints the process's DPI awareness and each monitor's real
scale factor. If it reports `scale: 1.0` everywhere you have tested, you have not
tested scaling — and a DPI-unaware build looks entirely healthy at 100%.

## macOS

An initial macOS backend ships alongside the Windows one: the same seven tools,
the same signed scopes and the same MCP surface, over Accessibility
(`AXUIElement`) instead of UI Automation, Core Graphics instead of `SendInput`,
ScreenCaptureKit instead of the DWM, and Vision instead of `Windows.Media.Ocr`.

Run `wincrust doctor` first. It reports the platform, both permissions and the
capability set, and it is read-only — it never prompts and never changes a
setting, because a diagnostic that fixes things cannot be trusted to describe
them.

```json
{ "accessibility": true, "screen_recording": true, "screenshot_api_available": true,
  "capabilities": { "launch": false, "window_capture": false, "ocr_click": false } }
```

**Permissions.** Grant *Accessibility* and *Screen Recording* in System Settings
→ Privacy & Security, then **restart the process** — macOS caches the decision
per process, so a grant made while wincrust is running does not reach it. Grant
them to whatever actually launches the binary: run it from a terminal and the
terminal is the trusted process, not wincrust.

**Window IDs are opaque and process-local.** `hwnd` on macOS is a counter, not a
`CGWindowID` and not a pointer. It is meaningful only to the wincrust process
that issued it and only while that process lives. Never persist one, and never
compute with one.

**macOS 14+** for screenshots. Accessibility works below that; capture does not.

**Keyboard.** Alphanumeric chords (`cmd+a`, `ctrl+shift+p`) currently require the
US or ABC layout, and are refused outright on any other rather than sending a
different shortcut. Named navigation keys are unaffected, and `type_keys` sends
Unicode directly — layout independent, clipboard untouched, and the way to type
anything that is not ASCII.

One asymmetry worth knowing before you build a loop on it: **a chord is only
processed while its application is frontmost.** macOS matches key equivalents in
the active app, so `cmd+a` posted to a background TextEdit is delivered and
discarded. `type_keys` is inserted either way. `act` says so in `detail` when it
notices, but it still reports `ok`, because `ok` has always meant *dispatched*
here, not *worked*.

### What is verified on macOS

On a Mac with both permissions granted, against Chrome (chosen because its
accessibility tree is the one least like a native app's) and a scratch TextEdit
window:

| | |
|---|---|
| `windows` | 34 windows across 9 applications, with names, pids and bounds |
| `displays` | 2056×1329, `scale 2.0`, `dpi 144` |
| `discover` (Chrome) | 400 elements in ~200 ms; 822 at depth 24 in ~360 ms |
| `find_text` | matched with `click_at` across 151 lines, ~1.8 s |
| `act` `type` | wrote the value, confirmed by re-reading the tree |
| `act` `type_keys` | `Tiếng Việt 漢喃 🦀` delivered intact |
| `act` `key` | `cmd+a` then `delete` cleared the document |
| `act` `click` | zoomed a window through `AXPress` |
| selector resolution | matched at the `exact` tier and acted |

Two refusals were exercised deliberately, because a guard nobody has seen fire
is a comment:

- Acting on a **stale lease** after the UI changed under it returned
  `identity_changed` and sent nothing. The value on screen was unchanged
  afterwards.
- Aiming a chord at a window that was **not its application's focused window**
  returned `no keyboard input sent` — before any input, not after.

An application that stops answering is the case worth knowing about, because
every AX request is synchronous on one shared engine thread. Requests are capped
process-wide, and the cap shrinks with the remaining budget so a single
unresponsive node cannot walk through it: against an application held in `T`
state, `discover` returns inside its five-second budget rather than the 8.2 s it
took when the cap was sized per request instead of per node. A window whose
application merely stopped answering keeps its handle and its outstanding
scopes — it is forgotten only when its application answers without listing it,
or exits.

Not verified: `scale 1.0`, more than one display, and any monitor sitting left
of or above the primary. `launch`, `window_capture` and `ocr_click` are reported
unsupported by `doctor` and are genuinely absent — `find_text` on macOS is a
desktop-wide survey, not a per-window crop.

## Limits

- Apps that draw their own UI expose no tree. Abaqus/CAE returns six elements, all window chrome.
  `find_text` is the fallback: OCR via `Windows.Media.Ocr`, returning text with screen coordinates.
  `act` can act on that too, with `allow_ocr` — off by default, because an OCR hit is a rectangle
  rather than a control, so acting means synthetic input at coordinates with none of the guarantees
  a control pattern gives. The result says `resolved_by: "ocr"` when that path was taken.
  It reads **15 of 15** Abaqus model-tree labels. Small dense text still degrades: Abaqus's menu bar
  sits at 6/9 words and **magnification does not move it** — upscaling helps when the recogniser's
  minimum feature size is the constraint, not when the source raster never captured the detail.
  Digit/letter confusion (`Steps (1)` → `Steps (I)`) is handled in the *matcher* rather than the
  recogniser, by folding glyph pairs OCR cannot separate. Where the application has a scripting API,
  prefer that over either.
- UAC *consent* prompts live on the Secure Desktop and are unreachable by any process, elevated or not.
- Must run in the interactive session. Session 0 has no desktop.

## Credits

Design informed by, but not derived from:
[desktop-touch-mcp](https://github.com/Harusame64/desktop-touch-mcp) (MIT) — leases and per-action
perception guards; [Windows-MCP](https://github.com/CursorTouch/Windows-MCP) (MIT) — tool surface and
the session-1 install pattern. No code from either is reused.

## Licence

Dual-licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE) or
  <https://www.apache.org/licenses/LICENSE-2.0>)
- MIT licence ([LICENSE-MIT](LICENSE-MIT) or <https://opensource.org/licenses/MIT>)

at your option. This is the conventional pairing for Rust crates: Apache-2.0
carries an explicit patent grant, MIT is the simpler and more permissive of the
two, and offering both lets a consumer pick whichever their own licensing
already accommodates.

Versions up to and including 0.2.0 were published under MIT alone; the dual
licence applies from the next release onward.

Unless you state otherwise, any contribution you submit for inclusion shall be
dual-licensed as above, with no additional terms.
