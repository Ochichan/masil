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

- **Desk Teal:** Marks the current focus, selected controls, interactive hover, focused borders, active dividers, and scrollbar thumbs.

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

At 100 columns by 24 rows or larger, the list and inspector appear side by side with a draggable one-cell divider. The list starts at 48 columns and both regions retain at least 34 columns. Below either wide threshold, the body shows the list or the opened inspector as one region.

The native side panel targets 34 columns. In compact mode below 60 columns, rows use two lines and filters and actions use two labeled columns across two rows when height permits. Below 18 rows, header, filters, search, actions, and footer each compress to one row. Below 28 columns or 9 rows, the desk shows only the minimum-size message and a working Close control.

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

The header keeps the view name and observer state visible. Wide layouts show full language, theme, help, and close labels. Narrow layouts retain keyboard hints and compact labels. Connected uses the success role; connecting and disconnected use the warning role with explicit text.

### Filters

All, Attention, Working, and Unavailable always carry observed-row counts. Attention includes only pending requests with available attention evidence. Working includes working and retrying activity. When the observer disconnects, Unavailable includes every retained row and the rows read as last-known evidence.

### Search field

Search matches observation identity, source, session, and pane identity. The field accepts at most 128 graphemes and 1,024 bytes, removes control characters, and keeps the cursor visible by rendered cell width. Clear changes only the query and selection view. It never changes acknowledgement state.

### Agent list

Rows sort pending attention first and then by stable observation identity. Selection follows that identity across live reordering. A selected row uses the raised fill and bold identity text. Compact rows reserve one line for identity and activity and one for the named attention type, count, and seen state. Approval and Question remain labeled rather than collapsing into an unlabeled number.

Unavailable rows use `State unavailable`. An offline row uses `Last known`. Freshness, configured but unverified bindings, invalidated associations, and exited or removed core state remain distinct. A disconnected observer leaves last-known evidence, never a completion claim.

### Evidence inspector

The inspector names activity, last activity, attention kind, Approval count, Question count, source, session, pane, native freshness, observation time, binding, foreground verification, core process, core freshness, and PTY generation. Native and core evidence stay separate. Supplemental detail can extend the inspector but cannot overwrite a newer complete row.

### Action controls

Idle controls use the raised fill. Focused or hovered controls use teal with dark accent text. A pressed control uses amber. Disabled controls return to the panel fill with muted text and remain labeled.

`Mark seen` requires a connected observer, available pending attention, a nonempty daemon epoch and attention revision, and no acknowledgement already in flight. It sends the displayed epoch and revision. It never grants provider approval or answers a question.

`Go to pane` requires a connected native helper, a valid binding, a running process, fresh core evidence, pane identity, PTY generation, and daemon epoch. An exited, removed, stale, or invalidated target disables navigation. `Expand` appears only in a compact native sidebar. `Retry` appears only while disconnected.

### Overlays, scrolling, and ownership

Help and the row action menu own keyboard and mouse input while open. A click outside closes the top overlay and consumes that click. List, inspector, and help scroll independently. Wheel input goes only to the region under the pointer.

A control activates only when press and release land on the same current target within two seconds. Row actions also retain the selected observation identity and revision across the gesture. A double-click uses the same row identity, button position, and a 500 millisecond window. Keyboard shortcuts call the same guarded actions as mouse controls.

### Feedback and restoration

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
