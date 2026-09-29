---
name: rmux Agent Desk
description: A terminal-native attention desk that keeps evidence beside the panes it describes.
colors:
  dark-canvas: "#0c1016"
  dark-panel: "#141a22"
  dark-raised: "#1f2731"
  dark-text: "#e6ebf1"
  dark-muted: "#9ba8b6"
  dark-accent: "#5bb5c2"
  dark-accent-text: "#051316"
  dark-warning: "#ebb85c"
  dark-danger: "#e86f75"
  dark-success: "#74c48a"
  dark-track: "#303a45"
  light-canvas: "#eef2f4"
  light-panel: "#fafbfb"
  light-raised: "#dce3e6"
  light-text: "#192229"
  light-muted: "#54646f"
  light-accent: "#00707f"
  light-accent-text: "#ffffff"
  light-warning: "#905400"
  light-danger: "#aa2a34"
  light-success: "#167036"
  light-track: "#becacf"
---

# Design System: rmux Agent Desk

## Overview

**Creative North Star: "Evidence Before Action"**

The rmux agent desk keeps agent attention beside the terminal panes it describes. Its stable list, evidence inspector, and guarded actions let a user find a waiting agent, inspect what rmux knows, mark the selected agent's displayed request set as seen, and request navigation to a configured pane.

The interface uses a compact terminal cell grid, charcoal surfaces, quiet borders, and a restrained teal accent. Every status keeps a text label. The design does not infer completion, approval, or a verified foreground agent from incomplete evidence.

**Key Characteristics:**

- A terminal-native list and inspector share one observation model.
- Dark, light, and host-terminal themes keep the same semantic roles.
- English and Korean expose the same controls and state meanings.
- Native tmux zoom expands the owned side panel without changing the desk's control model.
- The screen stays still at idle and redraws only after data, input, resize, or feedback changes.

## Colors

The default dark theme uses charcoal layers and a single teal accent. The light theme preserves the same roles with higher luminance. The terminal theme maps those roles to the host terminal's reset and ANSI colors, so it has no fixed hex palette.

### Primary

- **Desk Teal:** Marks the current focus, selected controls, interactive hover, focused borders and region titles, keyboard hints inside controls, the selected-row marker, active dividers, and scrollbar thumbs.

### Semantic

- **Working Green:** Marks connected status and observed working or retrying activity.
- **Pending Amber:** Marks unseen attention, connection transitions, pressed controls, and the minimum-size warning.
- **Unavailable Red:** Marks observations whose source, freshness, or binding cannot support a current state claim.

### Neutral

- **Charcoal Canvas:** Fills the terminal background outside panels.
- **Charcoal Panel:** Holds lists, fields, inspectors, overlays, and disabled controls.
- **Raised Charcoal:** Separates the selected row and idle controls from the panel.
- **Primary Text:** Carries identifiers, evidence, labels, and action names.
- **Muted Text and Track:** Carry secondary evidence, inactive borders, hints, and scrollbar tracks.

**The Text Owns Meaning Rule.** Color may reinforce a state, but `Fresh`, `State unavailable`, `Approval`, `Question`, `Seen`, and `Unseen` remain visible as words.

**The Quiet Border Rule.** Inactive one-cell borders use the track color. Teal appears on the region that owns focus or interaction.

## Typography

The product inherits the host terminal's font and cell metrics. It does not select or bundle a typeface, and it does not assume pixel sizes.

Normal terminal text carries most content. Bold marks the rmux title, selected identity, selected controls, and high-priority state labels. Help group labels and the raw supplemental-details label use uppercase text. English and Korean share the same hierarchy.

**The Cell Width Rule.** Clipping, cursor placement, and control width use rendered Unicode cell width. Search editing moves by grapheme and rejects control characters.

## Layout

The vertical order is fixed: header, filters, search, list or inspector body, action strip, and footer. The header owns product, view, connection, language, theme, help, and close. The footer owns focus hints, connection messages, and factual action receipts.

Outside the compact sidebar, header, filter, action, and footer text starts one cell in from the edge so it lines up with the text inside bordered regions. The header keeps one padding row above and below from 28 rows up; below that it is a single row, and the filter row moves down one row so a gap still separates it from the header.

At 100 columns by 24 rows or larger, the list and inspector appear side by side with a draggable one-cell divider. The divider sits on the canvas with a three-cell grip that turns teal on hover and fills teal while dragging. The list starts at 48 columns and both regions retain at least 34 columns. Below either wide threshold, the body shows the list or the opened inspector as one region.

The native side panel targets 34 columns. In compact mode below 60 columns, the header shows identity and connection on its first row and its controls on the second, rows use two lines, and filters and actions use two labeled columns across two rows when height permits. Below 18 rows, header, filters, search, actions, and footer each compress to one row. Below 28 columns or 9 rows, the desk shows only the minimum-size message and a working Close control.

Native zoom is a tmux pane operation owned by the sidebar. Expand and Restore change the tmux layout while the same rmux model, filters, selection, and action rules remain active.

**The Rendered Geometry Rule.** Each draw records only visible, clipped cell rectangles as hit targets. Resize clears old hit geometry and cancels press, drag, hover, and double-click state.

## Elevation & Depth

The terminal UI has no shadows, blur, or simulated depth. Canvas, panel, and raised fills separate layers. One-cell borders define lists, fields, inspectors, help, and context menus. Overlays clear their rectangular area before drawing above the desk.

There is no idle animation. Scroll, press, hover, focus, connection feedback, and acknowledgement progress update in response to events.

**The Flat State Rule.** Use tonal layers and borders for ownership. Do not add shadows or decorative motion to imply hierarchy.

## Shapes

All forms follow the terminal cell grid. Containers, controls, fields, menus, and hit targets are rectangular with square corners. Bordered regions use a one-cell outline. Scrollbars use a one-cell track and a proportional one-cell-wide thumb.

Clipped or zero-size geometry never receives input. Help is centered and bounded within the viewport. The context menu is clamped around its invocation point.

## Components

### Header and connection line

The header keeps the view name and observer state visible. Wide layouts show full language, theme, help, and close labels. Narrow layouts retain keyboard hints and compact labels. The title shortens in turn with the controls: the long connection text first, then the view name, then the product name. The short connection state stays, ending in `…` only at the 28-column minimum. Connected uses the success role; connecting and disconnected use the warning role with explicit text.

### Filters

All, Attention, Working, and Unavailable always carry observed-row counts. A filter with a zero count keeps its label in muted text. Control rows choose one label set for every control that fits the width: full labels, then short labels, then counts or keys alone. Labels never mix levels within one row. Attention includes only pending requests with available attention evidence. Working includes working and retrying activity. When the observer disconnects, Unavailable includes every retained row and the rows read as last-known evidence.

### Search field

Search matches observation identity, source, session, and pane identity. The field accepts at most 128 graphemes and 1,024 bytes, removes control characters, and keeps the cursor visible by rendered cell width. Clear changes only the query and selection view. It never changes acknowledgement state.

### Agent list

Rows sort pending attention first and then by stable observation identity. Selection follows that identity across live reordering. Wide rows place identity, activity, and attention in aligned columns sized from every filtered row, so columns stay put while scrolling. A selected row uses the raised fill, bold identity text, and a teal `›` marker; a hovered row shows a muted marker. Activity and attention carry separate colors: activity uses the working, pending, or unavailable role, and attention uses amber only for unseen pending requests. The word `pending` drops when the column cannot hold it; the kind, count, and seen state stay. When a row cannot hold the whole attention column beside a 12-cell identity prefix, every row switches to the two-line layout. A cut identity ends in `…`. Compact rows reserve one line for identity with right-aligned activity and one for the named attention type, count, and seen state. Approval and Question remain labeled rather than collapsing into an unlabeled number.

Unavailable rows use `State unavailable`. An offline row uses `Last known`. An empty list says why: connecting, no observed agents, no search match, or no pending requests. Freshness, configured but unverified bindings, invalidated associations, and exited or removed core state remain distinct. A disconnected observer leaves last-known evidence, never a completion claim.

### Evidence inspector

The inspector names activity, last activity, attention kind, Approval count, Question count, source, session, pane, native freshness, observation time, binding, foreground verification, core process, core freshness, and PTY generation. Native and core evidence stay separate under the `Native session` and `Core pane` headings. Labels sit in a muted column and values carry their semantic role. Values wrap under their own column and never lose trailing words; below 20 columns of value space, each label sits above its value. Supplemental detail can extend the inspector but cannot overwrite a newer complete row.

### Action controls

Idle controls use the raised fill with the keyboard key in bold teal before the label. Focused or hovered controls use teal with dark accent text. A pressed control uses amber. Disabled controls return to the panel fill with muted text and remain labeled.

`Mark seen` requires a connected observer, available pending attention, a nonempty daemon epoch and attention revision, and no acknowledgement already in flight. It sends the displayed epoch and revision. It never grants provider approval or answers a question.

`Go to pane` requires a connected native helper, a valid binding, a running process, fresh core evidence, pane identity, PTY generation, and daemon epoch. An exited, removed, stale, or invalidated target disables navigation. `Expand` appears only in a compact native sidebar. `Retry` appears only while disconnected.

### Overlays, scrolling, and ownership

Help and the row action menu own keyboard and mouse input while open. Help aligns keys in one column, wraps notes and narrow key names to its width, and places its Esc Close control in the top border. A narrow border shortens the title to Help and then the control to Esc. Menu items show their keyboard key at the right edge. A menu taller than the screen scrolls to keep the selection visible, marks hidden items with `↑` and `↓` in its border, and moves the selection with the wheel. A click outside closes the top overlay and consumes that click. List, inspector, and help scroll independently. Wheel input goes only to the region under the pointer.

A control activates only when press and release land on the same current target within two seconds. Row actions also retain the selected observation identity and revision across the gesture. A double-click uses the same row identity, button position, and a 500 millisecond window. Keyboard shortcuts call the same guarded actions as mouse controls.

### Feedback and restoration

A focus hint falls back to a short key list and then to `? Help`. Receipts and connection messages wrap over both footer rows and end in `…` if they still do not fit. In a two-row footer, the managed desk keeps its server status line on the last row, and messages wrap only in the row above it.

English is the default language and Korean is a full alternative. Receipts name only completed local or tmux actions, such as marking an identity seen, selecting the configured pane, expanding the sidebar, restoring the layout, or copying an ID to the named tmux buffer.

The terminal session owns alternate screen, raw input, cursor visibility, mouse capture, focus events, and bracketed paste. Close, error, and handled termination signals restore those modes before control returns to the shell.

## Do's and Don'ts

### Do

- **Do** keep evidence state visible in words, including Fresh, State unavailable, configured but unverified, invalidated, exited, and removed.
- **Do** label Approval and Question counts in both the list and inspector where space allows.
- **Do** preserve selection by observation identity and bind actions to the displayed attention revision.
- **Do** keep Unicode input bounded by grapheme count and byte count, then clip by terminal cell width.
- **Do** keep the 28 by 9 minimum view usable and the 34-column side panel readable.
- **Do** restore all terminal modes on normal, error, and handled-signal exits.

### Don't

- **Don't** treat Mark seen as provider approval, an answer, or a completion claim.
- **Don't** enable pane navigation without fresh matching core, pane, PTY generation, client-scope, and daemon identity evidence.
- **Don't** let background rows or regions receive input through an open overlay.
- **Don't** add an idle render loop, decorative animation, rounded web controls, or shadow depth.
- **Don't** import yututui artwork, music content, or product styling. It was used only as a craft and mouse-interaction reference.
- **Don't** treat review screenshots as shipping art. They are PTY-rendered test evidence with capture metadata.

## Native Agent Management UI

The shipped native management view reuses the desk's header, filters, Unicode search, identity-stable list, evidence inspector, action strip, context menu, themes, terminal restoration and responsive layouts. It switches the data source from agentd observations to the native `Manager` only for `rmux-agent agent ui` and its explicit sidebar.

The view owns one asynchronous inventory query at a time and schedules it once per second only while open. A projection comparison prevents unchanged inventory from triggering a redraw. Native actions run in bounded background tasks so pane reads and process operations do not block keyboard or mouse input.

Managed rows use **Needs input** for an untyped blocked state and **Returned idle** when a previously working run becomes idle. Both use shared acknowledgement revisions; neither asserts a question, approval, completion, or task success. Approval and Question stay at zero unless typed evidence exists. Start and resume receipts say that provider or native-session acceptance is unverified. Draft preparation names the `rmux-agent-draft` tmux buffer and explicitly says it was neither pasted nor submitted.

The managed action menu contains New agent, Go to pane, Details, Rename, Resume session, Prepare draft, Interrupt, Read screen and Close pane. Start, rename and draft use cell-grid forms. Interrupt and close require confirmations. Every form and confirmation consumes background input and exposes matching keyboard and mouse controls above the desk's existing highest overlay layer.

The optional 34-column management sidebar is created only by the explicit `agent sidebar` command. A per-pane marker binds it to the encoded native socket; repeated opening in the same window selects the live owned pane. Creation refuses an already zoomed window, and Expand rechecks the marker before toggling native pane zoom. The rmux menu opens the full management view in an ordinary native window and adds no default key binding.

원격 Agent 목록은 같은 관리 화면에 endpoint::name으로 표시한다. 서버별 연결 상태를 지속 표시하고 stale 행의 조작을 비활성화한다. 원격 새 실행에는 서버와 원격 cwd를 명시하며, 원래 TUI는 별도 native 연결 창에서 연다.
