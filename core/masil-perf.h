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

#ifndef MASIL_PERF_H
#define MASIL_PERF_H

/*
 * Latency markers for the performance gate. The server records raw samples
 * when MASIL_PERF_MARKERS names an absolute file path at server start and
 * writes them as JSON when the server exits. The same source is patched into
 * stock tmux (scripts/perf_marker_patch.py) so both products are measured at
 * the same places. Each hook is one line placed after an anchor line that is
 * identical in both trees.
 */

/*
 * Bump this string whenever a marker definition or a hook placement changes.
 * Both products write it into their marker file and the performance gate
 * rejects a file whose revision differs from this header.
 */
#define MASIL_PERF_REVISION "b6-2"

struct bufferevent;
struct tty;
struct client;

extern int	masil_perf_enabled;

void	masil_perf_on_init(void);
void	masil_perf_on_write(void);
void	masil_perf_on_input_begin(void);
void	masil_perf_on_input_read(void);
void	masil_perf_on_loop_drained(void);
void	masil_perf_on_key_write(struct bufferevent *);
void	masil_perf_on_pty_event(struct bufferevent *);
void	masil_perf_on_pty_free(struct bufferevent *);
void	masil_perf_on_pane_read_begin(void);
void	masil_perf_on_pane_read_end(void);
void	masil_perf_on_tty_queue(struct tty *);
void	masil_perf_on_tty_written(struct tty *);
void	masil_perf_on_tty_drained(struct tty *);
void	masil_perf_on_redraw_begin(struct client *);
void	masil_perf_on_redraw_end(void);

/* With markers off a hook costs one branch on masil_perf_enabled. */
#define MASIL_PERF_HOOK(call) do { \
	if (masil_perf_enabled) \
		call; \
} while (0)

/* The one hook that must run with markers off: it reads the environment. */
#define masil_perf_init() masil_perf_on_init()
#define masil_perf_write() MASIL_PERF_HOOK(masil_perf_on_write())
#define masil_perf_input_begin() MASIL_PERF_HOOK(masil_perf_on_input_begin())
#define masil_perf_input_read() MASIL_PERF_HOOK(masil_perf_on_input_read())
#define masil_perf_loop_drained() \
	MASIL_PERF_HOOK(masil_perf_on_loop_drained())
#define masil_perf_key_write(bev) MASIL_PERF_HOOK(masil_perf_on_key_write(bev))
#define masil_perf_pty_event(bev) MASIL_PERF_HOOK(masil_perf_on_pty_event(bev))
#define masil_perf_pty_free(bev) MASIL_PERF_HOOK(masil_perf_on_pty_free(bev))
#define masil_perf_pane_read_begin() \
	MASIL_PERF_HOOK(masil_perf_on_pane_read_begin())
#define masil_perf_pane_read_end() \
	MASIL_PERF_HOOK(masil_perf_on_pane_read_end())
#define masil_perf_tty_queue(tty) MASIL_PERF_HOOK(masil_perf_on_tty_queue(tty))
#define masil_perf_tty_written(tty) \
	MASIL_PERF_HOOK(masil_perf_on_tty_written(tty))
#define masil_perf_tty_drained(tty) \
	MASIL_PERF_HOOK(masil_perf_on_tty_drained(tty))
#define masil_perf_redraw_begin(c) \
	MASIL_PERF_HOOK(masil_perf_on_redraw_begin(c))
#define masil_perf_redraw_end() MASIL_PERF_HOOK(masil_perf_on_redraw_end())

#endif
