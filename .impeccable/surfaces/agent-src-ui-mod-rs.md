---
version: 1
slug: "agent-src-ui-mod-rs"
primary_target: "agent/src/ui/mod.rs"
related_targets: ["agent/src/ui/view.rs","agent/src/ui/input.rs"]
---

# masil agent desk: UI and interaction contract

Status: implementation contract. User choices: persistent side panel plus an on-demand management screen, actual TUI implementation before visual review, English default with Korean switching. Reference: yututui's rendered hit regions, modal input ownership, responsive layouts and mouse parity. Existing tmux commands, prefix and default key tables remain authoritative.

## Scope and visual authority

This is an Operate surface inside the existing terminal workspace. It extends terminal-native lists, fields, splitters and command menus. The user-selected layout is pinned; Impeccable surface seed `e2f9e8dc` does not reopen that decision. No music artwork, decorative animation or graphical terminal replacement is introduced.

The main task is to find an agent requiring attention, understand the evidence, mark the request as seen, and reach its configured pane. The UI never calls a provider approval API. It never calls an unverified native session a verified foreground agent. "Idle" never becomes "Done".

The selected visual grammar is a compact operations desk: a stable agent list, one inspector, action labels with visible keyboard hints, and a persistent connection line. Dark, light and terminal-color themes share the same semantic roles. Text uses the host terminal's font and cell grid. Color supplements visible words. A restrained accent identifies current selection and actionable focus; approval/question/unavailable each retain a text label.

## Information architecture

1. Header: product name, view name, connection status; controls for language, theme, help and close.
2. Filter strip: All, Attention, Working, Unavailable. Counts refer to observed rows, not completed tasks.
3. Search field: names/IDs/source/session, bounded editable Unicode text; explicit clear control.
4. Agent list: stable observation identity, source, activity, attention count and seen/unseen status. Selected identity survives live updates, sorting and resizing.
5. Inspector: selected observation's state, pending counts, source/session/pane identifiers, evidence freshness, binding explanation, and safe actions.
6. Footer: actionable hints for the currently focused region, reconnect/error/toast feedback.

The side panel uses the same model with compact rows and a short selected-item action strip. Full management space adds the inspector beside the list. It is not a second backend or a terminal emulator.

## Entry points and ownership

- `masil-agent --socket MANAGER ui` opens the management screen in the current terminal's alternate screen.
- Explicit `sidebar` opens a native side pane in a specified/current masil window. It preserves the original active pane and refuses to squeeze a terminal below the documented minimum. A same-manager panel in that window is reused rather than duplicated.
- Expand toggles tmux's native pane zoom on the owned sidebar. Restore returns to the original split layout. The expanded pane renders the full inspector. No permanent default binding is added.
- A sidebar is a shared native pane. All clients viewing the same tmux window see its layout. Opening/closing it is an explicit workspace action, and the UI labels that fact.
- Close/Escape restores the UI's terminal. An owned sidebar process exits and its pane is removed. Creation sets remain-on-exit off only on that newly owned pane, even when the user enables it globally.
- Native helpers refuse ambiguous clients, read-only clients, existing shared modal windows and mutations that would affect another attached client. The pinned tmux implementation shares popups across a window, so this UI uses its own menus and help overlays.
- Pane markers include the created PTY generation. A respawned former sidebar is never reused as a live UI. Existing user zoom is preserved by refusing sidebar creation until the user restores it.
- Optional user bindings are documented, never installed in `.tmux.conf` or default key tables.

## Responsive composition

| Space | Composition |
| --- | --- |
| Wide desktop | Agent list at left, draggable divider, full inspector at right; all primary controls visible |
| Ordinary 80-column terminal | Compact list with inspector reachable by Enter/Details; search/filter and action strip remain visible |
| Side panel, roughly 34 columns | Two-line rows, compact filters, selected item actions, full-screen expansion |
| Tiny terminal | Deliberate minimum-size view with connection information and a working Close control; no clipped invisible targets |

Every render calculates cell rectangles once and records exactly the controls it draws. Empty/zero-size/clipped targets do not receive input. After resize the old hit map and drag geometry are invalid. Inspector/list scroll positions are independent and clamped after filtering or resize. Labels and clipping use Unicode cell width, never byte count.

## Mouse contract

| Gesture/location | Result |
| --- | --- |
| Left press on row | Select that identity, focus list; never confirm or approve |
| Left double-click same row | Request guarded navigation to that configured pane when available |
| Right click row | Select row and open its context menu, clamped inside the viewport |
| Left release on button | Activate only if press and release belong to the same live target |
| Press button then drag outside | Cancel activation |
| Wheel over list | Scroll list; no action on the terminal or inspector underneath |
| Wheel over inspector | Scroll inspector only |
| Wheel over modal/menu | Route to the modal/menu; no click-through or background scrolling |
| Click scrollbar track | Page toward the click |
| Drag scrollbar thumb | Capture the drag until release, clamped even when pointer leaves the track |
| Drag list/inspector divider | Resize within minimum widths, with a visible active divider |
| Hover actionable control | Highlight its rendered target and show a specific hint; do not focus or act |
| Click filter | Select filter and preserve selected ID if still present |
| Click search | Focus input; place cursor by rendered cell position |
| Click Clear | Clear search without changing acknowledgement state |
| Click Details | Open/focus inspector; use existing evidence and request full details explicitly where needed |
| Click Mark seen | Send displayed daemon epoch and that observation's attention revision; never bulk-ack unseen changes |
| Click Go to pane | Guard core boot, pane ID, PTY generation and client scope before native focus |
| Click menu outside | Close it and consume the click |
| Click Help outside | Close help and consume the click |
| Escape | Dismiss the top overlay, then search editing/detail view, then leave the screen |
| Focus loss / resize / fresh unrelated press | Cancel captured press, drag and double-click history |
| Pointer outside known geometry | No action |
| Live row movement during gesture | Match target identity and captured revision, never the new row at the old index |
| Unsupported mouse reporting | Keyboard controls remain complete; help explains host-terminal selection behavior |

Double-click history is keyed by target identity, button, position and bounded time. A menu-opening press cannot become activation of a new overlay on its second click. Changes of daemon epoch cancel interactions immediately.

## Keyboard parity

Arrow keys and j/k move through the list; Home/End and PageUp/PageDown navigate long lists. Tab/Shift-Tab cycles visible control regions with clear focus. `/` focuses search, Enter opens details or performs the focused action, Escape backs out. `a` marks the selected pending request as seen, `g` requests pane navigation, `?` opens help, `l` changes language, `t` changes theme, and `q` closes when not editing text. Focused text editing uses arrows, Home/End, Backspace/Delete and Ctrl-U. Bracketed paste inserts plain bounded text rather than interpreting key shortcuts. These bindings are local to the management TUI, not tmux defaults.

Every mouse-only-looking control must have a keyboard route and a help entry. Text input owns printable shortcuts while focused. Destructive terminal/process actions are not exposed in this UI.

## Truthful states and feedback

| State | Presentation and available recovery |
| --- | --- |
| Connecting | Stable layout, "Connecting to observer"; no fabricated rows |
| Connected | Snapshot rows and source freshness shown separately |
| No matching rows | Explain current filter/search; Clear search and All controls |
| No pending attention | "No unseen requests" only when relevant sources are available; unavailable count remains visible |
| Native source unavailable | "State unavailable" with reason/last evidence; Mark seen disabled |
| Core association invalidated | Keep native evidence separate; pane navigation disabled until safe identity is available |
| Daemon disconnected | Preserve last rows as stale evidence; cancel interactions and disable ack/navigation; Retry/Close available |
| Daemon restarted | New epoch replaces identity context; preserve only harmless view preferences |
| Ack in flight | Selected action reads "Marking seen"; duplicate activation disabled, navigation/search still responsive |
| Ack accepted | "Marked <id> as seen"; backend projection remains authoritative |
| Ack rejected as stale | Explain that the request changed and refresh selection; never retry automatically against a new revision |
| Native focus rejected | Explain changed/removed target, wrong core or shared-focus conflict; do not claim navigation succeeded |
| Small terminal | Give exact minimum and a usable exit, not cropped controls |

Success receipts describe only completed local state changes. Native focus can report logical selection, never physical screen visibility. Clipboard operations, if exposed, name the actual destination and cannot claim unverified system clipboard delivery.

## Preferences and compatibility

English is the fresh-install default; Korean uses the same routes and control meanings. Theme/language can change without losing selection or entering a new screen. Session-only CLI overrides are supported. If preferences are persisted, use a bounded versioned UI-only file and atomic private writes; never rewrite provider or tmux configuration. Unsupported/corrupt files produce a recoverable session-only mode.

Native terminal modes are owned by an RAII guard: alternate screen, raw input, cursor visibility, mouse capture, focus and bracketed paste must be restored on normal close, error and handled termination signals. Host clipboard selection may require the terminal's override modifier while mouse capture is active; the help must say so without promising a universal modifier.

## Technical boundaries

- Ratatui/Crossterm render only the management UI; agent PTY bytes stay in the C core.
- Reuse validated complete `watch-agents` snapshots. A single latest-state channel is enough; intermediate UI snapshots may coalesce.
- Network/command work cannot block input or rendering. Bounded request/result channels and deadlines isolate failure.
- Draw only on changed data, interaction, resize or feedback deadline. No idle animation loop or provider polling.
- Mouse dispatch uses a typed last-rendered hit map and shared actions with keyboard dispatch. Z-order and modal ownership are explicit.
- Native helpers bind status.core.core_boot_id to the watch snapshot's daemon epoch. They compare that expected identity, target PTY generation and attached-client scope at the native command boundary. No shell interpolation of provider labels/IDs. Navigation out of an expanded owned sidebar restores its split layout in the guarded command queue before switching the client.
- New read-only core format callbacks may expose bridge boot/PTY identity and conservative focus-conflict evidence. Registered tmux commands and key tables stay unchanged.
- Test fixtures are isolated private core/manager processes, not the user's running sessions. Do not launch yututui or touch its configuration/audio.

## Acceptance and evidence

- Unit tests for mouse hit geometry, press/release capture, menu ownership, scrollbars, resize/focus loss, double-click identity, list stability and Unicode editing/clipping.
- Real PTY tests send SGR mouse events and keyboard events through the UI, capture rendered screens, resize and terminate; assert actual ack and native pane focus, not only reducer output.
- Core guard tests cover wrong boot, stale PTY, removed targets, readonly/shared clients and repeated sidebar opening/closing.
- Default tmux command/key/option comparison remains green.
- Capture wide, 80-column, sidebar and tiny layouts plus context menu/help, Korean, disconnected and empty states in one visual review batch. Apply one batched correction and confirm once before the independent Impeccable finish review.
- Record UI/daemon idle resource use separately and verify no idle render/output loop. Report tested host/platform limits rather than declaring every terminal perfect.
- Finish with the reviewer disposition, DESIGN.md documenting the actual built system, usage and validation artifacts.
