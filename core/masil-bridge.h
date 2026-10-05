/*
 * Copyright (c) 2026 masil contributors
 *
 * Permission to use, copy, modify, and distribute this software for any
 * purpose with or without fee is hereby granted, provided that the above
 * copyright notice and this permission notice appear in all copies.
 *
 * THE SOFTWARE IS PROVIDED "AS IS" AND THE AUTHOR DISCLAIMS ALL WARRANTIES
 * WITH REGARD TO THIS SOFTWARE INCLUDING ALL IMPLIED WARRANTIES OF
 * MERCHANTABILITY AND FITNESS. IN NO EVENT SHALL THE AUTHOR BE LIABLE FOR
 * ANY SPECIAL, DIRECT, INDIRECT, OR CONSEQUENTIAL DAMAGES OR ANY DAMAGES
 * WHATSOEVER RESULTING FROM LOSS OF USE, DATA OR PROFITS, WHETHER IN AN
 * ACTION OF CONTRACT, NEGLIGENCE OR OTHER TORTIOUS ACTION, ARISING OUT OF
 * OR IN CONNECTION WITH THE USE OR PERFORMANCE OF THIS SOFTWARE.
 */

#ifndef MASIL_BRIDGE_H
#define MASIL_BRIDGE_H

struct window_pane;
struct window;
struct client;
struct session;

void	masil_bridge_start(void);
void	masil_bridge_stop(void);
void	masil_bridge_server_exiting(void);
const char *masil_bridge_get_boot_id(void);
const char *masil_bridge_get_socket_path(void);
char	*masil_bridge_client_id(struct client *);
char	*masil_bridge_client_profile(struct client *);
void	 masil_bridge_window_active_changed(struct window *);
void	 masil_bridge_session_changed(struct session *);
void	 masil_bridge_client_session_changed(struct client *);

void	masil_bridge_pane_created(struct window_pane *);
void	masil_bridge_pane_destroyed(struct window_pane *);
void	masil_bridge_pane_state_changed(struct window_pane *);
void	masil_bridge_pty_changed(struct window_pane *);
void	masil_bridge_output_changed(struct window_pane *);
void	masil_bridge_geometry_changed(struct window_pane *);

#endif /* MASIL_BRIDGE_H */
