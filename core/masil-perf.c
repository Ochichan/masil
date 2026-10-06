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

#include <sys/types.h>

#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>
#include <unistd.h>

#include "tmux.h"
#include "masil-perf.h"

#ifndef MASIL_PERF_PRODUCT
#define MASIL_PERF_PRODUCT "masil"
#endif

#define PERF_SAMPLES 262144
#define PERF_SLOTS 1024 /* power of two */

enum perf_marker_id {
	PERF_INPUT_TO_PTY_QUEUE,
	PERF_KEY_TO_PTY_WRITE,
	PERF_PANE_OUTPUT_TO_TTY,
	PERF_REDRAW,
	PERF_MARKERS
};

static const char *perf_marker_names[PERF_MARKERS] = {
	"input_to_pty_queue",
	"key_to_pty_write",
	"pane_output_to_tty",
	"redraw"
};

struct perf_marker {
	uint32_t	*samples;
	uint64_t	 count;
	uint64_t	 dropped;
};

/* The earliest pending start for an object (a pane's PTY or a client tty). */
struct perf_slot {
	const void	*key;
	uint64_t	 start; /* 0 is none */
};

int			 masil_perf_enabled;
static char		*perf_path;
static int		 perf_written;
static struct perf_marker perf_markers[PERF_MARKERS];
static struct perf_slot	*perf_pty_slots;
static struct perf_slot	*perf_tty_slots;

/*
 * A tty read only queues keys. The PTY write happens later, in the command
 * queue drain at the top of server_loop. The earliest successful read start
 * stays pending until that drain ends (masil_perf_on_loop_drained), and key
 * writes made in the drain are attributed to it. send-keys, paste and other
 * commands that write to a pane in the same drain are attributed to the read
 * too.
 */
static uint64_t		 perf_input_start; /* start of the current read */
static uint64_t		 perf_key_pending; /* earliest unflushed read, 0 none */
static int		 perf_pane_active;
static uint64_t		 perf_pane_start;
static uint64_t		 perf_redraw_start;

/* Microseconds on the monotonic clock, offset by one so that 0 means unset. */
static uint64_t
perf_now(void)
{
	struct timespec	ts;

	clock_gettime(CLOCK_MONOTONIC, &ts);
	return ((uint64_t)ts.tv_sec * 1000000 + (uint64_t)ts.tv_nsec / 1000 + 1);
}

static void
perf_record(enum perf_marker_id id, uint64_t start)
{
	struct perf_marker	*m = &perf_markers[id];
	uint64_t		 elapsed = perf_now() - start;

	if (m->count >= PERF_SAMPLES) {
		m->dropped++;
		return;
	}
	m->samples[m->count++] = elapsed > UINT32_MAX ? UINT32_MAX :
	    (uint32_t)elapsed;
}

static struct perf_slot *
perf_slot(struct perf_slot *slots, const void *key, int create)
{
	size_t	i, n;

	i = ((uintptr_t)key >> 4) & (PERF_SLOTS - 1);
	for (n = 0; n < PERF_SLOTS; n++) {
		if (slots[i].key == key)
			return (&slots[i]);
		if (slots[i].key == NULL) {
			if (!create)
				return (NULL);
			slots[i].key = key;
			slots[i].start = 0;
			return (&slots[i]);
		}
		i = (i + 1) & (PERF_SLOTS - 1);
	}
	return (NULL);
}

void
masil_perf_on_init(void)
{
	const char	*env = getenv("MASIL_PERF_MARKERS");
	char		*path;
	u_int		 i;

	if (masil_perf_enabled || env == NULL)
		return;
	path = strdup(env);
	/* Panes and nested servers must not inherit the marker path. */
	environ_unset(global_environ, "MASIL_PERF_MARKERS");
	if (path == NULL || *path != '/') {
		free(path);
		return;
	}

	for (i = 0; i < PERF_MARKERS; i++) {
		perf_markers[i].samples = calloc(PERF_SAMPLES, sizeof(uint32_t));
		if (perf_markers[i].samples == NULL)
			goto fail;
	}
	perf_pty_slots = calloc(PERF_SLOTS, sizeof *perf_pty_slots);
	perf_tty_slots = calloc(PERF_SLOTS, sizeof *perf_tty_slots);
	perf_path = path;
	if (perf_pty_slots == NULL || perf_tty_slots == NULL)
		goto fail;
	masil_perf_enabled = 1;
	return;

fail:
	for (i = 0; i < PERF_MARKERS; i++) {
		free(perf_markers[i].samples);
		perf_markers[i].samples = NULL;
	}
	free(perf_pty_slots);
	free(perf_tty_slots);
	free(perf_path);
	perf_pty_slots = perf_tty_slots = NULL;
	perf_path = NULL;
}

/* Write the file once, through a temporary file in the same directory. */
void
masil_perf_on_write(void)
{
	FILE		*fp;
	char		*tmp;
	u_int		 i;
	uint64_t	 j;

	if (!masil_perf_enabled || perf_written)
		return;
	perf_written = 1;

	if (asprintf(&tmp, "%s.tmp.%ld", perf_path, (long)getpid()) == -1)
		return;
	if ((fp = fopen(tmp, "w")) == NULL) {
		free(tmp);
		return;
	}
	fprintf(fp, "{\"version\":1,\"product\":\"%s\","
	    "\"revision\":\"%s\","
	    "\"clock\":\"monotonic_us\",\"markers\":{", MASIL_PERF_PRODUCT,
	    MASIL_PERF_REVISION);
	for (i = 0; i < PERF_MARKERS; i++) {
		fprintf(fp, "%s\"%s\":{\"count\":%llu,\"dropped\":%llu,"
		    "\"samples_us\":[", i == 0 ? "" : ",", perf_marker_names[i],
		    (unsigned long long)perf_markers[i].count,
		    (unsigned long long)perf_markers[i].dropped);
		for (j = 0; j < perf_markers[i].count; j++) {
			fprintf(fp, "%s%u", j == 0 ? "" : ",",
			    (u_int)perf_markers[i].samples[j]);
		}
		fprintf(fp, "]}");
	}
	fprintf(fp, "}}\n");
	if (ferror(fp)) {
		fclose(fp);
		unlink(tmp);
	} else if (fclose(fp) != 0 || rename(tmp, perf_path) != 0)
		unlink(tmp);
	free(tmp);
}

/* Start of tty_read_callback. Keys count only once the read has succeeded. */
void
masil_perf_on_input_begin(void)
{
	perf_input_start = perf_now();
}

/* The read succeeded: keep the earliest pending start. */
void
masil_perf_on_input_read(void)
{
	if (perf_key_pending == 0)
		perf_key_pending = perf_input_start;
}

/* The command queue drain in server_loop is done. */
void
masil_perf_on_loop_drained(void)
{
	perf_key_pending = 0;
}

/* A key write into a pane's PTY buffer made while a read is pending. */
void
masil_perf_on_key_write(struct bufferevent *bev)
{
	struct perf_slot	*slot;

	if (perf_key_pending == 0)
		return;
	perf_record(PERF_INPUT_TO_PTY_QUEUE, perf_key_pending);

	slot = perf_slot(perf_pty_slots, bev, 1);
	if (slot == NULL)
		perf_markers[PERF_KEY_TO_PTY_WRITE].dropped++;
	else if (slot->start == 0)
		slot->start = perf_key_pending;
}

/* A pane's PTY event is freed: its pending start can never complete. */
void
masil_perf_on_pty_free(struct bufferevent *bev)
{
	struct perf_slot	*slot;

	slot = perf_slot(perf_pty_slots, bev, 0);
	if (slot != NULL && slot->start != 0) {
		slot->start = 0;
		perf_markers[PERF_KEY_TO_PTY_WRITE].dropped++;
	}
}

/* The PTY write callback runs once the output buffer is down to 0 bytes. */
static void
perf_pty_written(struct bufferevent *bev, __unused void *data)
{
	struct perf_slot	*slot;

	slot = perf_slot(perf_pty_slots, bev, 0);
	if (slot == NULL || slot->start == 0)
		return;
	if (evbuffer_get_length(bufferevent_get_output(bev)) != 0)
		return;
	perf_record(PERF_KEY_TO_PTY_WRITE, slot->start);
	slot->start = 0;
}

/* Install the write callback on a new pane PTY event (stock has none). */
void
masil_perf_on_pty_event(struct bufferevent *bev)
{
	bufferevent_data_cb	 readcb, writecb;
	bufferevent_event_cb	 eventcb;
	void			*arg;

	bufferevent_getcb(bev, &readcb, &writecb, &eventcb, &arg);
	if (writecb != NULL)
		return;
	bufferevent_setcb(bev, readcb, perf_pty_written, eventcb, arg);
	bufferevent_setwatermark(bev, EV_WRITE, 0, 0);
}

void
masil_perf_on_pane_read_begin(void)
{
	perf_pane_active = 1;
	perf_pane_start = perf_now();
}

void
masil_perf_on_pane_read_end(void)
{
	perf_pane_active = 0;
}

/* Output queued to a client tty while a pane read is handled. */
void
masil_perf_on_tty_queue(struct tty *tty)
{
	struct perf_slot	*slot;

	if (!perf_pane_active)
		return;
	slot = perf_slot(perf_tty_slots, tty, 1);
	if (slot == NULL)
		perf_markers[PERF_PANE_OUTPUT_TO_TTY].dropped++;
	else if (slot->start == 0)
		slot->start = perf_pane_start;
}

/* tty_block_maybe drained tty->out: the pending output never reaches the fd. */
void
masil_perf_on_tty_drained(struct tty *tty)
{
	struct perf_slot	*slot;

	slot = perf_slot(perf_tty_slots, tty, 0);
	if (slot != NULL && slot->start != 0) {
		slot->start = 0;
		perf_markers[PERF_PANE_OUTPUT_TO_TTY].dropped++;
	}
}

/* After a tty write: a sample once the output buffer is empty. */
void
masil_perf_on_tty_written(struct tty *tty)
{
	struct perf_slot	*slot;

	slot = perf_slot(perf_tty_slots, tty, 0);
	if (slot == NULL || slot->start == 0)
		return;
	if (EVBUFFER_LENGTH(tty->out) != 0)
		return;
	perf_record(PERF_PANE_OUTPUT_TO_TTY, slot->start);
	slot->start = 0;
}

/* Only full window redraws are sampled, not status or border redraws. */
void
masil_perf_on_redraw_begin(struct client *c)
{
	if (c->flags & CLIENT_REDRAWWINDOW)
		perf_redraw_start = perf_now();
	else
		perf_redraw_start = 0;
}

void
masil_perf_on_redraw_end(void)
{
	if (perf_redraw_start == 0)
		return;
	perf_record(PERF_REDRAW, perf_redraw_start);
	perf_redraw_start = 0;
}
