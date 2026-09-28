# Product

<!-- impeccable:product-schema 1 -->

## Platform

Terminal TUI on the existing tmux-derived Unix terminal core. This is not a web or mobile application; Impeccable's web/mobile platform enum does not describe this surface.

## Users

Developers running several original coding-agent TUIs alongside shells. They need to find waiting work, inspect why attention is needed, and return to the right pane without losing their existing tmux habits.

## Product Purpose

rmux combines tmux's terminal multiplexing with optional agent observation. Terminal input/output stays in the C core. The Rust observer tracks explicitly configured native sessions and reports only evidence it has verified.

## Operating Context

The user selected an always-visible side panel with an on-demand full management screen. Both run in the terminal. Existing tmux default keybindings remain intact. English is the default interface language with Korean switching. Mouse and keyboard must reach the same tasks.

## Capabilities and Constraints

- The current core supports native tmux session/window/pane behavior and mouse defaults.
- agentd provides current OpenCode observations, shared in-memory attention acknowledgement and bounded full-state streaming. Confirmation in rmux is not provider approval.
- Native-session/pane associations remain unverified. No prompt submission, provider approval or completion claim is available.
- UI reads the existing manager protocol. It must not place a renderer or parser in the agents' terminal byte path or add idle provider polling.
- Performance and terminal restoration are product requirements. Resize, lost mouse release, disconnected daemon and stale evidence must have defined behavior.
- A shared native side pane affects the tmux window layout for every attached client. Opening it is explicit; it must never be created by a status query or ordinary rmux startup.

## Brand Commitments

Use the name rmux. The user named yututui as the craft and mouse-interaction reference. Preserve terminal-native density, native tmux controls, honest state labels and legible English/Korean copy.

## Evidence on Hand

Existing protocol and validation records under docs/; source and screenshots from the user-authorized yututui reference. Its music content and artwork do not belong to rmux. No verified frontend identity, model-completion evidence or durable acknowledgement store exists yet.

## Product Principles

- Preserve the terminal workspace and its defaults.
- Make uncertain evidence visible and disable actions that require missing evidence.
- Give mouse users and keyboard users the same task outcomes.
- Retain selection by identity as live data changes.
- Keep inactive work inexpensive and interaction responsive.
