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
#include <sys/socket.h>
#include <sys/stat.h>
#include <sys/un.h>

#include <errno.h>
#include <fcntl.h>
#include <limits.h>
#include <stdarg.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>
#include <unistd.h>

#include "tmux.h"
#include "masil-bridge.h"
#include "yyjson.h"

#define MASIL_BRIDGE_RX_MAX		(8 * 1024)
#define MASIL_BRIDGE_TX_MAX		(64 * 1024)
#define MASIL_BRIDGE_CODEC_POOL		(512 * 1024)
#define MASIL_BRIDGE_TX_TOTAL		(512 * 1024)
#define MASIL_BRIDGE_COORDINATOR_HEADROOM (MASIL_BRIDGE_TX_MAX + 4)
#define MASIL_BRIDGE_CLIENTS		32
#define MASIL_BRIDGE_ORDINARY_CLIENTS	30
#define MASIL_BRIDGE_UNAUTHENTICATED	4
#define MASIL_BRIDGE_PAGE		64
#define MASIL_BRIDGE_ROWS		32
#define MASIL_BRIDGE_COLUMNS		240
#define MASIL_BRIDGE_TEXT		(16 * 1024)
#define MASIL_BRIDGE_TEXT_ENCODED		(40 * 1024)
#define MASIL_BRIDGE_BATCH_FRAMES		4
#define MASIL_BRIDGE_BATCH_BYTES	(32 * 1024)
#define MASIL_BRIDGE_BATCH_USEC		200
#define MASIL_BRIDGE_CLIENT_TIMEOUT	5
#define MASIL_BRIDGE_SNAPSHOT_RATE	64
#define MASIL_BRIDGE_SNAPSHOT_BYTES	(512 * 1024)
#define MASIL_BRIDGE_SNAPSHOT_BURST	(128 * 1024)
#define MASIL_BRIDGE_CREDIT_SCALE		1000000ULL
#define MASIL_BRIDGE_WATCH_PANES	64
#define MASIL_BRIDGE_WATCH_SLOTS	512
#define MASIL_BRIDGE_JOURNAL_EVENTS	4096
#define MASIL_BRIDGE_JOURNAL_VISITS	256
#define MASIL_BRIDGE_STOP_FLUSHES	(MASIL_BRIDGE_JOURNAL_EVENTS + 1)
#define MASIL_BRIDGE_DIRTY_BATCH	64
#define MASIL_BRIDGE_DIRTY_USEC		250000ULL

/* masil: validity bits keep native UINT_MAX ids distinct from JSON null. */
#define MASIL_BRIDGE_VIEW_SESSION	0x1
#define MASIL_BRIDGE_VIEW_WINDOW	0x2
#define MASIL_BRIDGE_VIEW_PANE		0x4

enum masil_bridge_event_reason {
	MASIL_BRIDGE_SCREEN_DIRTY,
	MASIL_BRIDGE_RESIZED,
	MASIL_BRIDGE_PTY_CHANGED,
	MASIL_BRIDGE_EXITED,
	MASIL_BRIDGE_REMOVED,
	/* masil: events below extend the watch stream in protocol 1.2. */
	MASIL_BRIDGE_CREATED,
	MASIL_BRIDGE_WINDOW_REMOVED,
	MASIL_BRIDGE_SESSION_REMOVED,
	MASIL_BRIDGE_CLIENT_VIEW,
	MASIL_BRIDGE_CLIENT_GONE,
	/* Protocol 1.2 terminal event: it has no object identifier. */
	MASIL_BRIDGE_SERVER_EXITING
};

struct masil_bridge_watch_slot {
	struct window_pane	*wp;
	u_int			 pane_id;
	uint64_t		 pty_generation;
	uint64_t		 screen_generation;
	uint64_t		 dirty_after;
	size_t			 subscribers;
	size_t			 used_position;
	int			 used;
	int			 dirty;
};

struct masil_bridge_journal_event {
	uint64_t		 seq;
	uint64_t		 pty_generation;
	uint64_t		 screen_generation;
	uint64_t		 client_serial;
	uint64_t		 view_revision;
	u_int			 pane_id;
	u_int			 window_id;
	u_int			 session_id;
	uint16_t		 slot;
	uint8_t			 reason;
	uint8_t			 valid;
};

struct masil_bridge_client {
	int			 fd;
	int			 hello_done;
	int			 coordinator;
	int			 coordinator_only;
	uint64_t		 action_epoch;
	int			 close_after_write;
	struct event		 event;
	struct event		 timeout_event;
	u_char			 rx[4 + MASIL_BRIDGE_RX_MAX];
	size_t			 rx_len;
	u_char			*tx;
	size_t			 tx_len;
	size_t			 tx_off;
	uint64_t		 stream_epoch;
	uint64_t		 watch_cursor;
	uint64_t		 watch_bits[MASIL_BRIDGE_WATCH_SLOTS / 64];
	uint16_t		 watch_slots[MASIL_BRIDGE_WATCH_PANES];
	size_t			 watch_count;
	int			 watching;
	int			 lifecycle_all;
	int			 clients_all;
	int			 watch_reschedule;
	struct masil_bridge_client *next;
};

struct masil_bridge_stats {
	uint64_t connects;
	uint64_t disconnects;
	uint64_t rx_frames;
	uint64_t tx_frames;
	uint64_t rx_bytes;
	uint64_t tx_bytes;
	uint64_t requests;
	uint64_t rejected;
	uint64_t protocol_errors;
	uint64_t inventory_requests;
	uint64_t snapshots;
	uint64_t rate_limited;
	uint64_t events;
	uint64_t coalesced;
	uint64_t gaps;
};

struct masil_json_builder {
	char	*buf;
	size_t	 len;
	size_t	 cap;
	int	 failed;
};

static int			 masil_bridge_enabled;
static int			 masil_bridge_path_derived;
static int			 masil_bridge_exiting;
static int			 masil_bridge_fd = -1;
static struct event		 masil_bridge_event;
static char			 masil_bridge_path[sizeof ((struct sockaddr_un *)0)->sun_path];
static dev_t			 masil_bridge_socket_dev;
static ino_t			 masil_bridge_socket_ino;
static char			 masil_bridge_boot_id[37];
static uint64_t			 masil_bridge_revision = 1;
static int			 masil_bridge_revision_exhausted;
static struct masil_bridge_client *masil_bridge_clients;
static size_t			 masil_bridge_client_count;
static size_t			 masil_bridge_unauthenticated;
static size_t			 masil_bridge_ordinary_clients;
static int			 masil_bridge_coordinator_connected;
static size_t			 masil_bridge_tx_bytes;
static uint64_t			 masil_bridge_snapshot_updated;
static uint64_t			 masil_bridge_snapshot_credit;
static uint64_t			 masil_bridge_snapshot_byte_credit;
static struct masil_bridge_watch_slot masil_bridge_watch_slots[MASIL_BRIDGE_WATCH_SLOTS];
static uint16_t			 masil_bridge_used_slots[MASIL_BRIDGE_WATCH_SLOTS];
static size_t			 masil_bridge_used_slot_count;
static size_t			 masil_bridge_watch_pane_refs;
static size_t			 masil_bridge_watch_clients;
static size_t			 masil_bridge_lifecycle_clients;
static size_t			 masil_bridge_view_clients;
static struct events_sink	*masil_bridge_lifecycle_sinks[2];
static struct events_sink	*masil_bridge_view_sinks[10];
static size_t			 masil_bridge_pending_dirty;
static struct masil_bridge_journal_event masil_bridge_journal[MASIL_BRIDGE_JOURNAL_EVENTS];
static size_t			 masil_bridge_journal_head;
static size_t			 masil_bridge_journal_count;
static uint64_t			 masil_bridge_event_seq;
static int			 masil_bridge_event_exhausted;
static uint64_t			 masil_bridge_stream_epoch;
static int			 masil_bridge_stream_exhausted;
static struct event		 masil_bridge_dirty_event;
static int			 masil_bridge_dirty_timer_active;
static uint64_t			 masil_bridge_dirty_timer_due;
static struct masil_bridge_stats masil_bridge_stats;
static u_char			 masil_bridge_codec_pool[MASIL_BRIDGE_CODEC_POOL];
static char			 masil_bridge_response[MASIL_BRIDGE_TX_MAX];

static void printflike(2, 3) masil_json_printf(struct masil_json_builder *,
    const char *, ...);
static void masil_bridge_json_id(struct masil_json_builder *, u_int, char,
    int);

static uint32_t
masil_bridge_get_u32(const u_char *p)
{
	return (((uint32_t)p[0] << 24) | ((uint32_t)p[1] << 16) |
	    ((uint32_t)p[2] << 8) | (uint32_t)p[3]);
}

static void
masil_bridge_put_u32(u_char *p, uint32_t value)
{
	p[0] = (value >> 24) & 0xff;
	p[1] = (value >> 16) & 0xff;
	p[2] = (value >> 8) & 0xff;
	p[3] = value & 0xff;
}

static uint64_t
masil_bridge_now_usec(void)
{
	struct timespec ts;

	if (clock_gettime(CLOCK_MONOTONIC, &ts) != 0)
		clock_gettime(CLOCK_REALTIME, &ts);
	return ((uint64_t)ts.tv_sec * 1000000ULL + ts.tv_nsec / 1000);
}

static void
masil_bridge_new_boot_id(void)
{
	u_char data[16];

	arc4random_buf(data, sizeof data);
	data[6] = (data[6] & 0x0f) | 0x40;
	data[8] = (data[8] & 0x3f) | 0x80;
	snprintf(masil_bridge_boot_id, sizeof masil_bridge_boot_id,
	    "%02x%02x%02x%02x-%02x%02x-%02x%02x-%02x%02x-"
	    "%02x%02x%02x%02x%02x%02x",
	    data[0], data[1], data[2], data[3], data[4], data[5], data[6],
	    data[7], data[8], data[9], data[10], data[11], data[12], data[13],
	    data[14], data[15]);
}

static int
masil_bridge_bump(uint64_t *value)
{
	if (*value == UINT64_MAX)
		return (-1);
	(*value)++;
	return (0);
}

static void
masil_json_putn(struct masil_json_builder *builder, const char *value,
    size_t length)
{
	if (builder->failed)
		return;
	if (length > builder->cap - builder->len) {
		builder->failed = 1;
		return;
	}
	memcpy(builder->buf + builder->len, value, length);
	builder->len += length;
}

static void
masil_json_puts(struct masil_json_builder *builder, const char *value)
{
	masil_json_putn(builder, value, strlen(value));
}

static void
masil_json_printf(struct masil_json_builder *builder, const char *fmt, ...)
{
	va_list ap;
	int	 length;

	if (builder->failed)
		return;
	va_start(ap, fmt);
	length = vsnprintf(builder->buf + builder->len,
	    builder->cap - builder->len, fmt, ap);
	va_end(ap);
	if (length < 0 || (size_t)length >= builder->cap - builder->len) {
		builder->failed = 1;
		return;
	}
	builder->len += length;
}

static void
masil_json_quote(struct masil_json_builder *builder, const char *value,
    size_t length)
{
	static const char hex[] = "0123456789abcdef";
	size_t		 i;
	u_char		 ch;
	char		 escaped[6];

	masil_json_puts(builder, "\"");
	for (i = 0; i < length; i++) {
		ch = value[i];
		switch (ch) {
		case '\"':
			masil_json_puts(builder, "\\\"");
			break;
		case '\\':
			masil_json_puts(builder, "\\\\");
			break;
		case '\b':
			masil_json_puts(builder, "\\b");
			break;
		case '\f':
			masil_json_puts(builder, "\\f");
			break;
		case '\n':
			masil_json_puts(builder, "\\n");
			break;
		case '\r':
			masil_json_puts(builder, "\\r");
			break;
		case '\t':
			masil_json_puts(builder, "\\t");
			break;
		default:
			if (ch < 0x20) {
				escaped[0] = '\\';
				escaped[1] = 'u';
				escaped[2] = '0';
				escaped[3] = '0';
				escaped[4] = hex[ch >> 4];
				escaped[5] = hex[ch & 0xf];
				masil_json_putn(builder, escaped, sizeof escaped);
			} else
				masil_json_putn(builder, (const char *)&value[i], 1);
			break;
		}
	}
	masil_json_puts(builder, "\"");
}

static size_t
masil_json_quoted_length(const char *value, size_t length)
{
	size_t i, total = 0;
	u_char ch;

	for (i = 0; i < length; i++) {
		ch = value[i];
		if (ch == '\"' || ch == '\\' || ch == '\b' || ch == '\f' ||
		    ch == '\n' || ch == '\r' || ch == '\t')
			total += 2;
		else if (ch < 0x20)
			total += 6;
		else
			total++;
	}
	return (total);
}

static void
masil_bridge_client_update(struct masil_bridge_client *client);
static void
masil_bridge_watch_unsubscribe(struct masil_bridge_client *client);

static void
masil_bridge_client_close(struct masil_bridge_client *client)
{
	struct masil_bridge_client **previous, *found;

	masil_bridge_watch_unsubscribe(client);
	event_del(&client->event);
	if (event_initialized(&client->timeout_event))
		event_del(&client->timeout_event);
	close(client->fd);
	if (client->tx != NULL) {
		masil_bridge_tx_bytes -= client->tx_len;
		free(client->tx);
	}
	if (!client->hello_done && masil_bridge_unauthenticated != 0)
		masil_bridge_unauthenticated--;
	/* masil: release role admission only after an authenticated close. */
	if (client->hello_done) {
		if (client->coordinator) {
			/* masil: closing a coordinator fences its action epoch. */
			masil_action_coordinator_close(client->action_epoch);
			masil_bridge_coordinator_connected = 0;
		} else if (masil_bridge_ordinary_clients != 0)
			masil_bridge_ordinary_clients--;
	}
	previous = &masil_bridge_clients;
	while ((found = *previous) != NULL) {
		if (found == client) {
			*previous = client->next;
			break;
		}
		previous = &found->next;
	}
	if (masil_bridge_client_count != 0)
		masil_bridge_client_count--;
	masil_bridge_stats.disconnects++;
	free(client);
}

static int
masil_bridge_queue(struct masil_bridge_client *client, const char *json,
    size_t length)
{
	size_t frame_length = length + 4, reserve = 0;

	if (length == 0 || length > MASIL_BRIDGE_TX_MAX || client->tx != NULL)
		return (-1);
	if (masil_bridge_coordinator_connected && !client->coordinator)
		reserve = MASIL_BRIDGE_COORDINATOR_HEADROOM;
	if (masil_bridge_tx_bytes > MASIL_BRIDGE_TX_TOTAL - reserve ||
	    frame_length > MASIL_BRIDGE_TX_TOTAL - reserve - masil_bridge_tx_bytes)
		return (-1);
	client->tx = xmalloc(frame_length);
	masil_bridge_put_u32(client->tx, length);
	memcpy(client->tx + 4, json, length);
	client->tx_len = frame_length;
	client->tx_off = 0;
	masil_bridge_tx_bytes += frame_length;
	masil_bridge_stats.tx_frames++;
	masil_bridge_client_update(client);
	return (0);
}

static int
masil_bridge_coordinator_response_ready(struct masil_bridge_client *client)
{
	return (client->tx == NULL &&
	    masil_bridge_tx_bytes <= MASIL_BRIDGE_TX_TOTAL -
	    MASIL_BRIDGE_COORDINATOR_HEADROOM);
}

static void
masil_bridge_client_timeout(__unused int fd, __unused short events, void *data)
{
	struct masil_bridge_client *client = data;

	masil_bridge_stats.protocol_errors++;
	masil_bridge_client_close(client);
}

static void
masil_bridge_client_deadline(struct masil_bridge_client *client)
{
	struct timeval timeout = { .tv_sec = MASIL_BRIDGE_CLIENT_TIMEOUT };
	int active;

	active = !client->hello_done || client->rx_len != 0 ||
	    client->tx != NULL;
	if (event_initialized(&client->timeout_event))
		event_del(&client->timeout_event);
	if (!active)
		return;
	evtimer_set(&client->timeout_event, masil_bridge_client_timeout, client);
	evtimer_add(&client->timeout_event, &timeout);
}

static int
masil_bridge_queue_builder(struct masil_bridge_client *client,
    struct masil_json_builder *builder)
{
	if (builder->failed || builder->len == 0)
		return (-1);
	return (masil_bridge_queue(client, builder->buf, builder->len));
}

static int
masil_bridge_error(struct masil_bridge_client *client, const char *request_id,
    size_t request_id_length, const char *code, const char *message)
{
	struct masil_json_builder builder = {
	    masil_bridge_response, 0, sizeof masil_bridge_response, 0
	};

	masil_json_puts(&builder, "{\"v\":1,\"kind\":\"error\"");
	if (request_id != NULL) {
		masil_json_puts(&builder, ",\"request_id\":");
		masil_json_quote(&builder, request_id, request_id_length);
	}
	masil_json_puts(&builder, ",\"code\":");
	masil_json_quote(&builder, code, strlen(code));
	masil_json_puts(&builder, ",\"message\":");
	masil_json_quote(&builder, message, strlen(message));
	masil_json_puts(&builder, "}");
	masil_bridge_stats.rejected++;
	return (masil_bridge_queue_builder(client, &builder));
}

static int
masil_bridge_key_allowed(const char *key, size_t length,
    const char *const *allowed, size_t allowed_count)
{
	size_t i;

	for (i = 0; i < allowed_count; i++) {
		if (strlen(allowed[i]) == length &&
		    memcmp(key, allowed[i], length) == 0)
			return (1);
	}
	return (0);
}

static int
masil_bridge_schema(yyjson_val *root, const char *const *allowed,
    size_t allowed_count, const char **code)
{
	yyjson_obj_iter	 iter;
	yyjson_val	*key;
	const char	*keys[32];
	size_t		 lengths[32], count = 0, i, length;
	const char	*string;

	if (!yyjson_is_obj(root)) {
		*code = "invalid_request";
		return (-1);
	}
	if (yyjson_obj_size(root) > 32) {
		*code = "too_many_fields";
		return (-1);
	}
	iter = yyjson_obj_iter_with(root);
	while ((key = yyjson_obj_iter_next(&iter)) != NULL) {
		string = yyjson_get_str(key);
		length = yyjson_get_len(key);
		for (i = 0; i < count; i++) {
			if (lengths[i] == length &&
			    memcmp(keys[i], string, length) == 0) {
				*code = "duplicate_field";
				return (-1);
			}
		}
		if (!masil_bridge_key_allowed(string, length, allowed,
		    allowed_count)) {
			*code = "unknown_field";
			return (-1);
		}
		keys[count] = string;
		lengths[count++] = length;
	}
	return (0);
}

static int
masil_bridge_parse_decimal(yyjson_val *value, uint64_t *result)
{
	const char *string;
	size_t length, i;
	uint64_t number = 0, digit;

	if (!yyjson_is_str(value))
		return (-1);
	string = yyjson_get_str(value);
	length = yyjson_get_len(value);
	if (length == 0 || length > 20)
		return (-1);
	for (i = 0; i < length; i++) {
		if (string[i] < '0' || string[i] > '9')
			return (-1);
		digit = string[i] - '0';
		if (number > (UINT64_MAX - digit) / 10)
			return (-1);
		number = number * 10 + digit;
	}
	*result = number;
	return (0);
}

static int
masil_bridge_hello(struct masil_bridge_client *client, const char *request_id,
    size_t request_id_length, yyjson_val *root)
{
	struct masil_json_builder builder = {
	    masil_bridge_response, 0, sizeof masil_bridge_response, 0
	};
	yyjson_val	*value;
	uint64_t	 action_epoch = 0;
	int		 coordinator = 0;

	/* masil: two transport slots remain unavailable to ordinary clients. */
	value = yyjson_obj_get(root, "role");
	if (value != NULL) {
		if (!yyjson_is_str(value) ||
		    !yyjson_equals_str(value, "coordinator"))
			return (masil_bridge_error(client, request_id,
			    request_id_length, "invalid_role",
			    "role must be coordinator when present"));
		coordinator = 1;
	}
	if (coordinator && masil_bridge_coordinator_connected) {
		client->close_after_write = 1;
		return (masil_bridge_error(client, request_id, request_id_length,
		    "coordinator_exists", "a coordinator is already connected"));
	}
	/* masil: a reservation lapses once a coordinator has connected. */
	if (!coordinator && client->coordinator_only &&
	    !masil_bridge_coordinator_connected) {
		client->close_after_write = 1;
		return (masil_bridge_error(client, request_id, request_id_length,
		    "coordinator_required",
		    "this reserved connection must use role coordinator"));
	}
	if (!coordinator &&
	    masil_bridge_ordinary_clients >= MASIL_BRIDGE_ORDINARY_CLIENTS) {
		client->close_after_write = 1;
		return (masil_bridge_error(client, request_id, request_id_length,
		    "connection_limit", "ordinary connection limit reached"));
	}
	if (coordinator && (action_epoch = masil_action_coordinator_open()) == 0)
		return (masil_bridge_error(client, request_id, request_id_length,
		    "epoch_exhausted", "an action epoch is not available"));

	masil_json_puts(&builder,
	    "{\"v\":1,\"kind\":\"hello\",\"request_id\":");
	masil_json_quote(&builder, request_id, request_id_length);
	masil_json_puts(&builder, ",\"core_boot_id\":");
	masil_json_quote(&builder, masil_bridge_boot_id,
	    strlen(masil_bridge_boot_id));
	masil_json_puts(&builder,
	    ",\"negotiated_version\":{\"major\":1,\"minor\":2},"
	    "\"capabilities\":{\"inventory\":true,\"snapshot\":true,"
	    "\"stats\":true,\"watch\":true,\"events\":true,"
	    "\"lifecycle\":true,\"clients\":true,\"server_exiting\":true,");
	if (coordinator)
		masil_json_printf(&builder,
		    "\"actions\":true,\"dispatch_epoch\":\"%llu\","
		    "\"submit\":false},",
		    (unsigned long long)action_epoch);
	else
		/* masil: ordinary hello keeps its protocol-1.2 key set unchanged. */
		masil_json_puts(&builder, "\"actions\":false,\"submit\":false},");
	masil_json_puts(&builder,
	    "\"limits\":{\"rx_frame\":8192,\"tx_frame\":65536,"
	    "\"json_depth\":8,\"inventory_page\":64,"
	    "\"snapshot_rows\":32,\"snapshot_columns\":240,"
	    "\"snapshot_text\":16384}}");
	if (masil_bridge_queue_builder(client, &builder) != 0) {
		if (coordinator)
			masil_action_coordinator_close(action_epoch);
		return (-1);
	}
	client->coordinator = coordinator;
	client->action_epoch = action_epoch;
	client->hello_done = 1;
	if (coordinator)
		masil_bridge_coordinator_connected = 1;
	else
		masil_bridge_ordinary_clients++;
	if (masil_bridge_unauthenticated != 0)
		masil_bridge_unauthenticated--;
	return (0);
}

static int
masil_bridge_inventory(struct masil_bridge_client *client,
    const char *request_id, size_t request_id_length, yyjson_val *root)
{
	struct masil_json_builder builder = {
	    masil_bridge_response, 0, sizeof masil_bridge_response, 0
	};
	struct window_pane	*wp, lookup, *page[MASIL_BRIDGE_PAGE + 1];
	yyjson_val		*value;
	uint64_t		 requested_revision, cursor = 0;
	size_t		 count = 0, i, shown;
	int		 has_cursor = 0, has_revision = 0;

	value = yyjson_obj_get(root, "cursor");
	if (masil_bridge_revision_exhausted)
		return (masil_bridge_error(client, request_id, request_id_length,
		    "generation_exhausted", "inventory revision cannot be reused"));
	if (value != NULL) {
		if (masil_bridge_parse_decimal(value, &cursor) != 0 ||
		    cursor > UINT_MAX)
			return (masil_bridge_error(client, request_id,
			    request_id_length, "invalid_cursor",
			    "cursor must be a decimal pane id"));
		has_cursor = 1;
	}
	value = yyjson_obj_get(root, "revision");
	if (value != NULL) {
		if (masil_bridge_parse_decimal(value, &requested_revision) != 0)
			return (masil_bridge_error(client, request_id,
			    request_id_length, "invalid_revision",
			    "revision must be a decimal string"));
		has_revision = 1;
	}
	if (has_cursor && !has_revision)
		return (masil_bridge_error(client, request_id, request_id_length,
		    "revision_required", "continuation requires revision"));
	if (has_revision && requested_revision != masil_bridge_revision)
		return (masil_bridge_error(client, request_id, request_id_length,
		    "resync_required", "inventory revision changed"));

	if (!has_cursor)
		wp = RB_MIN(window_pane_tree, &all_window_panes);
	else if (cursor == UINT_MAX)
		wp = NULL;
	else {
		memset(&lookup, 0, sizeof lookup);
		lookup.id = cursor + 1;
		wp = RB_NFIND(window_pane_tree, &all_window_panes, &lookup);
	}
	while (wp != NULL) {
		page[count++] = wp;
		if (count == nitems(page))
			break;
		wp = RB_NEXT(window_pane_tree, &all_window_panes, wp);
	}
	shown = count > MASIL_BRIDGE_PAGE ? MASIL_BRIDGE_PAGE : count;
	masil_json_puts(&builder,
	    "{\"v\":1,\"kind\":\"inventory\",\"request_id\":");
	masil_json_quote(&builder, request_id, request_id_length);
	masil_json_printf(&builder, ",\"core_boot_id\":\"%s\","
	    "\"revision\":\"%llu\",\"panes\":[",
	    masil_bridge_boot_id, (unsigned long long)masil_bridge_revision);
	for (i = 0; i < shown; i++) {
		wp = page[i];
		if (wp->masil_generation_exhausted)
			return (masil_bridge_error(client, request_id,
			    request_id_length, "generation_exhausted",
			    "pane generations cannot be reused"));
		if (i != 0)
			masil_json_puts(&builder, ",");
		masil_json_printf(&builder,
		    "{\"pane_id\":\"%%%u\",\"pty_generation\":\"%llu\","
		    "\"screen_generation\":\"%llu\",\"width\":%u,"
		    "\"height\":%u,\"dead\":%s}", wp->id,
		    (unsigned long long)wp->masil_pty_generation,
		    (unsigned long long)wp->masil_screen_generation,
		    screen_size_x(&wp->base), screen_size_y(&wp->base),
		    (wp->flags & (PANE_EXITED|PANE_DESTROYED)) ? "true" : "false");
	}
	masil_json_puts(&builder, "],\"next_cursor\":");
	if (count > MASIL_BRIDGE_PAGE)
		masil_json_printf(&builder, "\"%u\"", page[shown - 1]->id);
	else
		masil_json_puts(&builder, "null");
	masil_json_printf(&builder, ",\"complete\":%s}",
	    count > MASIL_BRIDGE_PAGE ? "false" : "true");
	masil_bridge_stats.inventory_requests++;
	return (masil_bridge_queue_builder(client, &builder));
}

static int
masil_bridge_pane_id(yyjson_val *value, u_int *pane_id)
{
	const char	*string;
	size_t		 length, i;
	uint64_t	 number = 0;

	if (!yyjson_is_str(value))
		return (-1);
	string = yyjson_get_str(value);
	length = yyjson_get_len(value);
	if (length < 2 || length > 11 || string[0] != '%')
		return (-1);
	for (i = 1; i < length; i++) {
		if (string[i] < '0' || string[i] > '9')
			return (-1);
		number = number * 10 + string[i] - '0';
		if (number > UINT_MAX)
			return (-1);
	}
	*pane_id = number;
	return (0);
}

static const char *
masil_bridge_reason_string(enum masil_bridge_event_reason reason)
{
	switch (reason) {
	case MASIL_BRIDGE_SCREEN_DIRTY:
		return ("screen_dirty");
	case MASIL_BRIDGE_RESIZED:
		return ("resized");
	case MASIL_BRIDGE_PTY_CHANGED:
		return ("pty_changed");
	case MASIL_BRIDGE_EXITED:
		return ("exited");
	case MASIL_BRIDGE_REMOVED:
		return ("removed");
	case MASIL_BRIDGE_CREATED:
		return ("created");
	case MASIL_BRIDGE_WINDOW_REMOVED:
		return ("window_removed");
	case MASIL_BRIDGE_SESSION_REMOVED:
		return ("session_removed");
	case MASIL_BRIDGE_CLIENT_VIEW:
		return ("client_view");
	case MASIL_BRIDGE_CLIENT_GONE:
		return ("client_gone");
	case MASIL_BRIDGE_SERVER_EXITING:
		return ("server_exiting");
	}
	return ("removed");
}

static struct masil_bridge_watch_slot *
masil_bridge_watch_slot(struct window_pane *wp)
{
	struct masil_bridge_watch_slot *slot;
	size_t index;

	if (wp->masil_watch_slot == 0)
		return (NULL);
	index = wp->masil_watch_slot - 1;
	if (index >= MASIL_BRIDGE_WATCH_SLOTS)
		return (NULL);
	slot = &masil_bridge_watch_slots[index];
	if (!slot->used || slot->wp != wp || slot->pane_id != wp->id)
		return (NULL);
	return (slot);
}

static void
masil_bridge_watch_slot_refresh(struct masil_bridge_watch_slot *slot)
{
	if (slot->wp == NULL)
		return;
	slot->pty_generation = slot->wp->masil_pty_generation;
	slot->screen_generation = slot->wp->masil_screen_generation;
}

static void
masil_bridge_watch_wake(void)
{
	struct masil_bridge_client *client;

	for (client = masil_bridge_clients; client != NULL; client = client->next) {
		if (client->watching && client->tx == NULL)
			event_active(&client->event, EV_READ, 1);
	}
}

static void
masil_bridge_watch_fail_closed(void)
{
	struct masil_bridge_client *client, *next;

	for (client = masil_bridge_clients; client != NULL; client = next) {
		next = client->next;
		if (client->watching)
			masil_bridge_client_close(client);
	}
}

/* masil: reserve one fixed journal record for any protocol 1.2 event. */
static struct masil_bridge_journal_event *
masil_bridge_journal_new(enum masil_bridge_event_reason reason)
{
	struct masil_bridge_journal_event *record;
	size_t index;

	if (masil_bridge_event_exhausted)
		return (NULL);
	if (masil_bridge_event_seq == UINT64_MAX) {
		masil_bridge_event_exhausted = 1;
		masil_bridge_watch_fail_closed();
		return (NULL);
	}
	if (masil_bridge_journal_count < MASIL_BRIDGE_JOURNAL_EVENTS) {
		index = (masil_bridge_journal_head +
		    masil_bridge_journal_count) % MASIL_BRIDGE_JOURNAL_EVENTS;
		masil_bridge_journal_count++;
	} else {
		index = masil_bridge_journal_head;
		masil_bridge_journal_head = (masil_bridge_journal_head + 1) %
		    MASIL_BRIDGE_JOURNAL_EVENTS;
	}
	record = &masil_bridge_journal[index];
	memset(record, 0, sizeof *record);
	record->seq = ++masil_bridge_event_seq;
	record->slot = UINT16_MAX;
	record->reason = reason;
	masil_bridge_stats.events++;
	return (record);
}

static void
masil_bridge_journal_finish(void)
{
	masil_bridge_watch_wake();
}

/* masil: lifecycle panes without explicit scope use the sentinel slot. */
static void
masil_bridge_journal_append_pane(struct masil_bridge_watch_slot *slot,
    struct window_pane *wp, enum masil_bridge_event_reason reason)
{
	struct masil_bridge_journal_event *record;

	record = masil_bridge_journal_new(reason);
	if (record == NULL)
		return;
	if (slot != NULL) {
		masil_bridge_watch_slot_refresh(slot);
		record->pane_id = slot->pane_id;
		record->pty_generation = slot->pty_generation;
		record->screen_generation = slot->screen_generation;
		record->slot = slot - masil_bridge_watch_slots;
	} else if (wp != NULL) {
		record->pane_id = wp->id;
		record->pty_generation = wp->masil_pty_generation;
		record->screen_generation = wp->masil_screen_generation;
	}
	record->valid |= MASIL_BRIDGE_VIEW_PANE;
	masil_bridge_journal_finish();
}

static void
masil_bridge_journal_append_window(u_int window_id)
{
	struct masil_bridge_journal_event *record;

	if (masil_bridge_exiting)
		return;
	record = masil_bridge_journal_new(MASIL_BRIDGE_WINDOW_REMOVED);
	if (record == NULL)
		return;
	record->window_id = window_id;
	record->valid |= MASIL_BRIDGE_VIEW_WINDOW;
	masil_bridge_journal_finish();
}

static void
masil_bridge_journal_append_session(u_int session_id)
{
	struct masil_bridge_journal_event *record;

	if (masil_bridge_exiting)
		return;
	record = masil_bridge_journal_new(MASIL_BRIDGE_SESSION_REMOVED);
	if (record == NULL)
		return;
	record->session_id = session_id;
	record->valid |= MASIL_BRIDGE_VIEW_SESSION;
	masil_bridge_journal_finish();
}

static void
masil_bridge_journal_append_client(struct client *c,
    enum masil_bridge_event_reason reason)
{
	struct masil_bridge_journal_event *record;

	record = masil_bridge_journal_new(reason);
	if (record == NULL)
		return;
	record->client_serial = c->masil_serial;
	record->view_revision = c->masil_view_revision;
	if (reason == MASIL_BRIDGE_CLIENT_VIEW) {
		record->session_id = c->masil_view_session_id;
		record->window_id = c->masil_view_window_id;
		record->pane_id = c->masil_view_pane_id;
		record->valid = c->masil_view_valid;
	}
	masil_bridge_journal_finish();
}

static void masil_bridge_dirty_callback(int, short, void *);

static void
masil_bridge_dirty_schedule(uint64_t due)
{
	struct timeval timeout;
	uint64_t now, delay;

	if (masil_bridge_dirty_timer_active &&
	    due >= masil_bridge_dirty_timer_due)
		return;
	if (masil_bridge_dirty_timer_active)
		event_del(&masil_bridge_dirty_event);
	now = masil_bridge_now_usec();
	delay = due > now ? due - now : 0;
	timeout.tv_sec = delay / 1000000;
	timeout.tv_usec = delay % 1000000;
	evtimer_add(&masil_bridge_dirty_event, &timeout);
	masil_bridge_dirty_timer_active = 1;
	masil_bridge_dirty_timer_due = due;
}

static void
masil_bridge_dirty_rearm(void)
{
	struct masil_bridge_watch_slot *slot;
	uint64_t due = UINT64_MAX;
	size_t i;

	if (masil_bridge_dirty_timer_active) {
		event_del(&masil_bridge_dirty_event);
		masil_bridge_dirty_timer_active = 0;
	}
	if (masil_bridge_pending_dirty == 0)
		return;
	for (i = 0; i < masil_bridge_used_slot_count; i++) {
		slot = &masil_bridge_watch_slots[masil_bridge_used_slots[i]];
		if (slot->dirty && slot->dirty_after < due)
			due = slot->dirty_after;
	}
	if (due != UINT64_MAX)
		masil_bridge_dirty_schedule(due);
}

static void
masil_bridge_dirty_cancel(struct masil_bridge_watch_slot *slot)
{
	if (!slot->dirty)
		return;
	slot->dirty = 0;
	if (masil_bridge_pending_dirty != 0)
		masil_bridge_pending_dirty--;
	masil_bridge_stats.coalesced++;
	masil_bridge_dirty_rearm();
}

static void
masil_bridge_dirty_callback(__unused int fd, __unused short events,
    __unused void *data)
{
	struct masil_bridge_watch_slot *slot;
	uint64_t now = masil_bridge_now_usec();
	size_t i, emitted = 0;

	masil_bridge_dirty_timer_active = 0;
	for (i = 0; i < masil_bridge_used_slot_count; i++) {
		slot = &masil_bridge_watch_slots[masil_bridge_used_slots[i]];
		if (!slot->dirty || slot->dirty_after > now)
			continue;
		slot->dirty = 0;
		if (masil_bridge_pending_dirty != 0)
			masil_bridge_pending_dirty--;
		slot->dirty_after = now + MASIL_BRIDGE_DIRTY_USEC;
		masil_bridge_journal_append_pane(slot, slot->wp,
		    MASIL_BRIDGE_SCREEN_DIRTY);
		if (++emitted == MASIL_BRIDGE_DIRTY_BATCH ||
		    masil_bridge_now_usec() - now >= MASIL_BRIDGE_BATCH_USEC)
			break;
	}
	masil_bridge_dirty_rearm();
}

static int
masil_bridge_watch_allocate(struct window_pane *wp, uint16_t *index)
{
	struct masil_bridge_watch_slot *slot;
	size_t i;

	if ((slot = masil_bridge_watch_slot(wp)) != NULL) {
		*index = slot - masil_bridge_watch_slots;
		return (0);
	}
	for (i = 0; i < MASIL_BRIDGE_WATCH_SLOTS; i++) {
		if (!masil_bridge_watch_slots[i].used)
			break;
	}
	if (i == MASIL_BRIDGE_WATCH_SLOTS)
		return (-1);
	slot = &masil_bridge_watch_slots[i];
	memset(slot, 0, sizeof *slot);
	slot->used = 1;
	slot->wp = wp;
	slot->pane_id = wp->id;
	slot->pty_generation = wp->masil_pty_generation;
	slot->screen_generation = wp->masil_screen_generation;
	slot->used_position = masil_bridge_used_slot_count;
	masil_bridge_used_slots[masil_bridge_used_slot_count++] = i;
	wp->masil_watch_slot = i + 1;
	*index = i;
	return (0);
}

static void
masil_bridge_watch_slot_release(uint16_t index)
{
	struct masil_bridge_watch_slot *slot = &masil_bridge_watch_slots[index];
	struct masil_bridge_watch_slot *moved;
	uint16_t moved_index;
	size_t position;

	if (!slot->used || slot->subscribers != 0)
		return;
	masil_bridge_dirty_cancel(slot);
	if (slot->wp != NULL && slot->wp->masil_watch_slot == index + 1)
		slot->wp->masil_watch_slot = 0;
	position = slot->used_position;
	masil_bridge_used_slot_count--;
	if (position != masil_bridge_used_slot_count) {
		moved_index = masil_bridge_used_slots[masil_bridge_used_slot_count];
		masil_bridge_used_slots[position] = moved_index;
		moved = &masil_bridge_watch_slots[moved_index];
		moved->used_position = position;
	}
	memset(slot, 0, sizeof *slot);
}

/* masil: upstream lifecycle sinks exist only while requested by a watcher. */
static void
masil_bridge_lifecycle_event(__unused const char *name,
    struct event_payload *ep, __unused void *data)
{
	struct window	*w;
	struct session	*s;

	if ((w = event_payload_get_window(ep, "window")) != NULL)
		masil_bridge_journal_append_window(w->id);
	else if ((s = event_payload_get_session(ep, "session")) != NULL)
		masil_bridge_journal_append_session(s->id);
}

static void
masil_bridge_lifecycle_enable(void)
{
	masil_bridge_lifecycle_sinks[0] = events_add_sink("window-closed",
	    masil_bridge_lifecycle_event, NULL);
	masil_bridge_lifecycle_sinks[1] = events_add_sink("session-closed",
	    masil_bridge_lifecycle_event, NULL);
}

static void
masil_bridge_lifecycle_disable(void)
{
	size_t i;

	for (i = 0; i < nitems(masil_bridge_lifecycle_sinks); i++) {
		events_remove_sink(masil_bridge_lifecycle_sinks[i]);
		masil_bridge_lifecycle_sinks[i] = NULL;
	}
}

/* masil: cache the client view tuple without retaining tmux object pointers. */
static int
masil_bridge_client_attached(struct client *c)
{
	return ((c->flags & (CLIENT_ATTACHED|CLIENT_DEAD)) == CLIENT_ATTACHED &&
	    c->session != NULL);
}

static u_char
masil_bridge_client_view_tuple(struct client *c, u_int *session_id,
    u_int *window_id, u_int *pane_id)
{
	struct session	*s = c->session;
	struct window	*w;

	*session_id = *window_id = *pane_id = 0;
	if (s == NULL)
		return (0);
	*session_id = s->id;
	if (s->curw == NULL || (w = s->curw->window) == NULL)
		return (MASIL_BRIDGE_VIEW_SESSION);
	*window_id = w->id;
	if (w->active == NULL)
		return (MASIL_BRIDGE_VIEW_SESSION|MASIL_BRIDGE_VIEW_WINDOW);
	*pane_id = w->active->id;
	return (MASIL_BRIDGE_VIEW_SESSION|MASIL_BRIDGE_VIEW_WINDOW|
	    MASIL_BRIDGE_VIEW_PANE);
}

static void
masil_bridge_client_recompute(struct client *c, int publish_unknown)
{
	u_int	 session_id, window_id, pane_id;
	u_char	 valid;

	/* masil: command clients and clients detaching from a session have no view. */
	if (!masil_bridge_client_attached(c))
		return;
	valid = masil_bridge_client_view_tuple(c, &session_id, &window_id,
	    &pane_id);
	if (!c->masil_view_known) {
		c->masil_view_session_id = session_id;
		c->masil_view_window_id = window_id;
		c->masil_view_pane_id = pane_id;
		c->masil_view_valid = valid;
		c->masil_view_known = 1;
		c->masil_gone_emitted = 0;
		if (!publish_unknown)
			return;
	} else if (c->masil_view_valid == valid &&
	    c->masil_view_session_id == session_id &&
	    c->masil_view_window_id == window_id &&
	    c->masil_view_pane_id == pane_id)
		return;
	else {
		c->masil_view_session_id = session_id;
		c->masil_view_window_id = window_id;
		c->masil_view_pane_id = pane_id;
		c->masil_view_valid = valid;
	}
	if (c->masil_view_revision == UINT64_MAX) {
		masil_bridge_watch_fail_closed();
		return;
	}
	c->masil_view_revision++;
	masil_bridge_journal_append_client(c, MASIL_BRIDGE_CLIENT_VIEW);
}

static void
masil_bridge_view_event(const char *name, struct event_payload *ep,
    __unused void *data)
{
	struct client *c;

	if (strcmp(name, "client-detached") == 0 ||
	    strcmp(name, "client-closed") == 0) {
		c = event_payload_get_client(ep, "client");
		if (c != NULL && c->masil_view_known &&
		    !c->masil_gone_emitted) {
			/* masil: the tombstone consumes the last published view once. */
			c->masil_gone_emitted = 1;
			c->masil_view_known = 0;
			masil_bridge_journal_append_client(c,
			    MASIL_BRIDGE_CLIENT_GONE);
		}
		return;
	}
	TAILQ_FOREACH(c, &clients, entry) {
		if (masil_bridge_client_attached(c))
			masil_bridge_client_recompute(c, 1);
	}
}

static void
masil_bridge_view_enable(void)
{
	static const char *const names[] = {
	    "client-attached", "client-detached", "client-closed",
	    "client-session-changed", "session-window-changed",
	    "window-pane-changed", "window-linked", "window-unlinked",
	    "client-resized", "client-active"
	};
	struct client	*c;
	size_t		 i;

	/* Establish a no-event baseline for clients that predate the subscriber. */
	TAILQ_FOREACH(c, &clients, entry) {
		if (masil_bridge_client_attached(c))
			masil_bridge_client_recompute(c, 0);
	}
	for (i = 0; i < nitems(names); i++)
		masil_bridge_view_sinks[i] = events_add_sink(names[i],
		    masil_bridge_view_event, NULL);
}

static void
masil_bridge_view_disable(void)
{
	struct client	*c;
	size_t		 i;

	for (i = 0; i < nitems(masil_bridge_view_sinks); i++) {
		events_remove_sink(masil_bridge_view_sinks[i]);
		masil_bridge_view_sinks[i] = NULL;
	}
	TAILQ_FOREACH(c, &clients, entry) {
		c->masil_view_known = 0;
		c->masil_gone_emitted = 0;
	}
}

/* masil: cover active-pane changes that suppress upstream notifications. */
void
masil_bridge_window_active_changed(struct window *w)
{
	struct client	*c;
	struct session	*s;

	if (!masil_bridge_enabled || masil_bridge_view_clients == 0)
		return;
	TAILQ_FOREACH(c, &clients, entry) {
		if (!masil_bridge_client_attached(c))
			continue;
		s = c->session;
		if (s->curw != NULL && s->curw->window == w)
			masil_bridge_client_recompute(c, 1);
	}
}

/* masil: cover direct or silent current-window changes for one session. */
void
masil_bridge_session_changed(struct session *s)
{
	struct client *c;

	if (!masil_bridge_enabled || masil_bridge_view_clients == 0)
		return;
	TAILQ_FOREACH(c, &clients, entry) {
		if (masil_bridge_client_attached(c) && c->session == s)
			masil_bridge_client_recompute(c, 1);
	}
}

static void
masil_bridge_client_session_callback(__unused int fd, __unused short events,
    void *data)
{
	struct client *c = data;

	if (masil_bridge_enabled && masil_bridge_view_clients != 0 &&
	    masil_bridge_client_attached(c))
		masil_bridge_client_recompute(c, 1);
	server_client_unref(c);
}

/* masil: initial attach flags are set after server_client_set_session returns. */
void
masil_bridge_client_session_changed(struct client *c)
{
	if (!masil_bridge_enabled || masil_bridge_view_clients == 0 ||
	    c->session == NULL)
		return;
	if (masil_bridge_client_attached(c)) {
		masil_bridge_client_recompute(c, 1);
		return;
	}
	c->references++;
	event_once(-1, EV_TIMEOUT, masil_bridge_client_session_callback, c, NULL);
}

static void
masil_bridge_watch_unsubscribe(struct masil_bridge_client *client)
{
	struct masil_bridge_watch_slot *slot;
	size_t i;
	uint16_t index;

	if (!client->watching)
		return;
	client->watching = 0;
	/* masil: tear down upstream sinks on the last matching subscriber. */
	if (client->lifecycle_all) {
		client->lifecycle_all = 0;
		if (masil_bridge_lifecycle_clients != 0)
			masil_bridge_lifecycle_clients--;
		if (masil_bridge_lifecycle_clients == 0)
			masil_bridge_lifecycle_disable();
	}
	if (client->clients_all) {
		client->clients_all = 0;
		if (masil_bridge_view_clients != 0)
			masil_bridge_view_clients--;
		if (masil_bridge_view_clients == 0)
			masil_bridge_view_disable();
	}
	if (masil_bridge_watch_clients != 0)
		masil_bridge_watch_clients--;
	for (i = 0; i < client->watch_count; i++) {
		index = client->watch_slots[i];
		slot = &masil_bridge_watch_slots[index];
		if (slot->subscribers != 0)
			slot->subscribers--;
		if (masil_bridge_watch_pane_refs != 0)
			masil_bridge_watch_pane_refs--;
		masil_bridge_watch_slot_release(index);
	}
	client->watch_count = 0;
	memset(client->watch_bits, 0, sizeof client->watch_bits);
	if (masil_bridge_watch_clients == 0) {
		masil_bridge_journal_head = 0;
		masil_bridge_journal_count = 0;
	}
}

static int
masil_bridge_watch(struct masil_bridge_client *client, const char *request_id,
    size_t request_id_length, yyjson_val *root)
{
	struct masil_json_builder builder = {
	    masil_bridge_response, 0, sizeof masil_bridge_response, 0
	};
	struct window_pane *panes[MASIL_BRIDGE_WATCH_PANES], *wp;
	struct masil_bridge_watch_slot *slot;
	struct client *c;
	yyjson_val *value, *pane_ids, *item;
	u_int ids[MASIL_BRIDGE_WATCH_PANES];
	uint16_t index;
	uint64_t epoch, fence;
	size_t count, i, j, new_slots = 0;
	int lifecycle_all = 0, clients_all = 0, view_enabled = 0;

	if (client->watching)
		return (masil_bridge_error(client, request_id, request_id_length,
		    "stream_active", "connection already has an active watch"));
	value = yyjson_obj_get(root, "expected_core_boot_id");
	if (!yyjson_is_str(value) ||
	    yyjson_get_len(value) != strlen(masil_bridge_boot_id) ||
	    memcmp(yyjson_get_str(value), masil_bridge_boot_id,
	    strlen(masil_bridge_boot_id)) != 0)
		return (masil_bridge_error(client, request_id, request_id_length,
		    "boot_mismatch", "core boot id does not match"));
	/* masil: lifecycle or client observation may have an empty pane scope. */
	value = yyjson_obj_get(root, "lifecycle");
	if (value != NULL) {
		if (!yyjson_is_str(value) || !yyjson_equals_str(value, "all"))
			return (masil_bridge_error(client, request_id,
			    request_id_length, "invalid_lifecycle",
			    "lifecycle must be all when present"));
		lifecycle_all = 1;
	}
	value = yyjson_obj_get(root, "clients");
	if (value != NULL) {
		if (!yyjson_is_bool(value))
			return (masil_bridge_error(client, request_id,
			    request_id_length, "invalid_clients",
			    "clients must be a boolean"));
		clients_all = yyjson_get_bool(value);
	}
	pane_ids = yyjson_obj_get(root, "pane_ids");
	if (!yyjson_is_arr(pane_ids) ||
	    (count = yyjson_arr_size(pane_ids)) > MASIL_BRIDGE_WATCH_PANES ||
	    (count == 0 && !lifecycle_all && !clients_all))
		return (masil_bridge_error(client, request_id, request_id_length,
		    "invalid_pane_ids",
		    "pane_ids may be empty only with lifecycle all or clients true"));
	if (masil_bridge_revision_exhausted || masil_bridge_event_exhausted ||
	    masil_bridge_stream_exhausted)
		return (masil_bridge_error(client, request_id, request_id_length,
		    "generation_exhausted", "watch sequence cannot be reused"));
	for (i = 0; i < count; i++) {
		item = yyjson_arr_get(pane_ids, i);
		if (masil_bridge_pane_id(item, &ids[i]) != 0)
			return (masil_bridge_error(client, request_id,
			    request_id_length, "invalid_pane_id",
			    "pane_ids entries must have the form %N"));
		for (j = 0; j < i; j++) {
			if (ids[j] == ids[i])
				return (masil_bridge_error(client, request_id,
				    request_id_length, "duplicate_pane_id",
				    "pane_ids entries must be unique"));
		}
		wp = window_pane_find_by_id(ids[i]);
		if (wp == NULL)
			return (masil_bridge_error(client, request_id,
			    request_id_length, "target_gone",
			    "watched pane does not exist"));
		if (wp->masil_generation_exhausted)
			return (masil_bridge_error(client, request_id,
			    request_id_length, "generation_exhausted",
			    "pane generations cannot be reused"));
		panes[i] = wp;
		if (masil_bridge_watch_slot(wp) == NULL)
			new_slots++;
	}
	if (new_slots > MASIL_BRIDGE_WATCH_SLOTS - masil_bridge_used_slot_count)
		return (masil_bridge_error(client, request_id, request_id_length,
		    "observation_capacity_exceeded",
		    "global watched pane capacity is exhausted"));
	if (masil_bridge_stream_epoch == UINT64_MAX) {
		masil_bridge_stream_exhausted = 1;
		return (masil_bridge_error(client, request_id, request_id_length,
		    "generation_exhausted", "stream epoch cannot be reused"));
	}
	/*
	 * masil: publish or refresh the client baseline before taking the fence.
	 * The event loop is single threaded, so the ACK snapshot and cursor name
	 * one state; subsequent changes enter the journal after fence_seq.
	 */
	if (clients_all) {
		if (masil_bridge_view_clients == 0) {
			masil_bridge_view_enable();
			view_enabled = 1;
		} else {
			TAILQ_FOREACH(c, &clients, entry) {
				if (masil_bridge_client_attached(c))
					masil_bridge_client_recompute(c, 1);
			}
		}
	}
	epoch = masil_bridge_stream_epoch + 1;
	fence = masil_bridge_event_seq;
	masil_json_puts(&builder,
	    "{\"v\":1,\"kind\":\"watch\",\"request_id\":");
	masil_json_quote(&builder, request_id, request_id_length);
	masil_json_printf(&builder,
	    ",\"core_boot_id\":\"%s\",\"stream_epoch\":\"%llu\","
	    "\"fence_seq\":\"%llu\",\"scope_revision\":\"%llu\","
	    "\"complete\":true,\"panes\":[", masil_bridge_boot_id,
	    (unsigned long long)epoch, (unsigned long long)fence,
	    (unsigned long long)masil_bridge_revision);
	for (i = 0; i < count; i++) {
		wp = panes[i];
		if (i != 0)
			masil_json_puts(&builder, ",");
		masil_json_printf(&builder,
		    "{\"pane_id\":\"%%%u\",\"pty_generation\":\"%llu\","
		    "\"screen_generation\":\"%llu\",\"width\":%u,"
		    "\"height\":%u,\"dead\":%s}", wp->id,
		    (unsigned long long)wp->masil_pty_generation,
		    (unsigned long long)wp->masil_screen_generation,
		    screen_size_x(&wp->base), screen_size_y(&wp->base),
		    (wp->flags & (PANE_EXITED|PANE_DESTROYED)) ? "true" : "false");
	}
	masil_json_puts(&builder, "]");
	if (clients_all) {
		masil_json_puts(&builder, ",\"clients\":[");
		j = 0;
		TAILQ_FOREACH(c, &clients, entry) {
			if (!masil_bridge_client_attached(c) ||
			    !c->masil_view_known)
				continue;
			if (j++ != 0)
				masil_json_puts(&builder, ",");
			masil_json_printf(&builder,
			    "{\"client_id\":\"%s:%llu\","
			    "\"view_revision\":\"%llu\",\"session_id\":",
			    masil_bridge_boot_id,
			    (unsigned long long)c->masil_serial,
			    (unsigned long long)c->masil_view_revision);
			masil_bridge_json_id(&builder, c->masil_view_session_id, '$',
			    c->masil_view_valid & MASIL_BRIDGE_VIEW_SESSION);
			masil_json_puts(&builder, ",\"window_id\":");
			masil_bridge_json_id(&builder, c->masil_view_window_id, '@',
			    c->masil_view_valid & MASIL_BRIDGE_VIEW_WINDOW);
			masil_json_puts(&builder, ",\"pane_id\":");
			masil_bridge_json_id(&builder, c->masil_view_pane_id, '%',
			    c->masil_view_valid & MASIL_BRIDGE_VIEW_PANE);
			masil_json_puts(&builder, "}");
		}
		masil_json_puts(&builder, "]");
	}
	masil_json_puts(&builder, "}");
	if (builder.failed) {
		if (view_enabled)
			masil_bridge_view_disable();
		return (masil_bridge_error(client, request_id, request_id_length,
		    "response_too_large",
		    "watch acknowledgement exceeds response size limit"));
	}

	client->watching = 1;
	client->stream_epoch = epoch;
	client->watch_cursor = fence;
	masil_bridge_stream_epoch = epoch;
	masil_bridge_watch_clients++;
	client->lifecycle_all = lifecycle_all;
	if (lifecycle_all && masil_bridge_lifecycle_clients++ == 0)
		masil_bridge_lifecycle_enable();
	client->clients_all = clients_all;
	if (clients_all)
		masil_bridge_view_clients++;
	for (i = 0; i < count; i++) {
		if (masil_bridge_watch_allocate(panes[i], &index) != 0) {
			masil_bridge_watch_unsubscribe(client);
			return (-1);
		}
		slot = &masil_bridge_watch_slots[index];
		slot->subscribers++;
		masil_bridge_watch_pane_refs++;
		client->watch_slots[client->watch_count++] = index;
		client->watch_bits[index / 64] |= 1ULL << (index % 64);
	}
	if (masil_bridge_queue_builder(client, &builder) != 0) {
		masil_bridge_watch_unsubscribe(client);
		return (-1);
	}
	return (0);
}

static int
masil_bridge_event_visible(struct masil_bridge_client *client,
    struct masil_bridge_journal_event *record)
{
	switch (record->reason) {
	case MASIL_BRIDGE_SCREEN_DIRTY:
	case MASIL_BRIDGE_RESIZED:
		break;
	case MASIL_BRIDGE_PTY_CHANGED:
	case MASIL_BRIDGE_EXITED:
	case MASIL_BRIDGE_REMOVED:
		if (client->lifecycle_all)
			return (1);
		break;
	case MASIL_BRIDGE_CREATED:
	case MASIL_BRIDGE_WINDOW_REMOVED:
	case MASIL_BRIDGE_SESSION_REMOVED:
		return (client->lifecycle_all);
	case MASIL_BRIDGE_CLIENT_VIEW:
	case MASIL_BRIDGE_CLIENT_GONE:
		return (client->clients_all);
	case MASIL_BRIDGE_SERVER_EXITING:
		/* A shutdown is terminal for every watch scope. */
		return (1);
	}
	if (record->slot == UINT16_MAX)
		return (0);
	return ((client->watch_bits[record->slot / 64] &
	    (1ULL << (record->slot % 64))) != 0);
}

/* masil: append a nullable native id using its protocol sigil. */
static void
masil_bridge_json_id(struct masil_json_builder *builder, u_int id, char sigil,
    int valid)
{
	if (!valid)
		masil_json_puts(builder, "null");
	else
		masil_json_printf(builder, "\"%c%u\"", sigil, id);
}

static int
masil_bridge_watch_pump(struct masil_bridge_client *client)
{
	struct masil_json_builder builder = {
	    masil_bridge_response, 0, sizeof masil_bridge_response, 0
	};
	struct masil_bridge_journal_event *record;
	uint64_t first, last, next;
	size_t offset, index, visits = 0;

	client->watch_reschedule = 0;
	if (!client->watching || client->tx != NULL ||
	    masil_bridge_journal_count == 0)
		return (0);
	first = masil_bridge_journal[masil_bridge_journal_head].seq;
	last = masil_bridge_event_seq;
	if (client->watch_cursor < first - 1) {
		masil_json_printf(&builder,
		    "{\"v\":1,\"kind\":\"gap\",\"core_boot_id\":\"%s\","
		    "\"stream_epoch\":\"%llu\",\"after_seq\":\"%llu\","
		    "\"first_available_seq\":\"%llu\",\"last_seq\":\"%llu\","
		    "\"code\":\"resync_required\"}", masil_bridge_boot_id,
		    (unsigned long long)client->stream_epoch,
		    (unsigned long long)client->watch_cursor,
		    (unsigned long long)first, (unsigned long long)last);
		masil_bridge_stats.gaps++;
		if (masil_bridge_queue_builder(client, &builder) != 0)
			return (-1);
		client->close_after_write = 1;
		return (0);
	}
	while (client->watch_cursor < last &&
	    visits < MASIL_BRIDGE_JOURNAL_VISITS) {
		next = client->watch_cursor + 1;
		offset = next - first;
		if (offset >= masil_bridge_journal_count)
			break;
		index = (masil_bridge_journal_head + offset) %
		    MASIL_BRIDGE_JOURNAL_EVENTS;
		record = &masil_bridge_journal[index];
		client->watch_cursor = record->seq;
		visits++;
		if (!masil_bridge_event_visible(client, record))
			continue;
		/* masil: all event kinds share the existing push envelope. */
		masil_json_printf(&builder,
		    "{\"v\":1,\"kind\":\"event\",\"core_boot_id\":\"%s\","
		    "\"stream_epoch\":\"%llu\",\"event_seq\":\"%llu\","
		    "\"reason\":\"%s\"",
		    masil_bridge_boot_id,
		    (unsigned long long)client->stream_epoch,
		    (unsigned long long)record->seq,
		    masil_bridge_reason_string(record->reason));
		switch (record->reason) {
		case MASIL_BRIDGE_SCREEN_DIRTY:
		case MASIL_BRIDGE_RESIZED:
		case MASIL_BRIDGE_PTY_CHANGED:
		case MASIL_BRIDGE_EXITED:
		case MASIL_BRIDGE_REMOVED:
		case MASIL_BRIDGE_CREATED:
			masil_json_printf(&builder,
			    ",\"pane_id\":\"%%%u\",\"pty_generation\":\"%llu\","
			    "\"screen_generation\":\"%llu\"", record->pane_id,
			    (unsigned long long)record->pty_generation,
			    (unsigned long long)record->screen_generation);
			break;
		case MASIL_BRIDGE_WINDOW_REMOVED:
			masil_json_printf(&builder, ",\"window_id\":\"@%u\"",
			    record->window_id);
			break;
		case MASIL_BRIDGE_SESSION_REMOVED:
			masil_json_printf(&builder, ",\"session_id\":\"$%u\"",
			    record->session_id);
			break;
		case MASIL_BRIDGE_CLIENT_VIEW:
			masil_json_printf(&builder,
			    ",\"client_id\":\"%s:%llu\",\"view_revision\":\"%llu\","
			    "\"session_id\":", masil_bridge_boot_id,
			    (unsigned long long)record->client_serial,
			    (unsigned long long)record->view_revision);
			masil_bridge_json_id(&builder, record->session_id, '$',
			    record->valid & MASIL_BRIDGE_VIEW_SESSION);
			masil_json_puts(&builder, ",\"window_id\":");
			masil_bridge_json_id(&builder, record->window_id, '@',
			    record->valid & MASIL_BRIDGE_VIEW_WINDOW);
			masil_json_puts(&builder, ",\"pane_id\":");
			masil_bridge_json_id(&builder, record->pane_id, '%',
			    record->valid & MASIL_BRIDGE_VIEW_PANE);
			break;
		case MASIL_BRIDGE_CLIENT_GONE:
			masil_json_printf(&builder, ",\"client_id\":\"%s:%llu\"",
			    masil_bridge_boot_id,
			    (unsigned long long)record->client_serial);
			break;
		case MASIL_BRIDGE_SERVER_EXITING:
			break;
		}
		masil_json_puts(&builder, "}");
		return (masil_bridge_queue_builder(client, &builder));
	}
	if (client->watch_cursor < last)
		client->watch_reschedule = 1;
	return (0);
}

static int
masil_bridge_snapshot_admit(size_t estimated_bytes)
{
	uint64_t now, elapsed, add;
	uint64_t request_max = MASIL_BRIDGE_SNAPSHOT_RATE *
	    MASIL_BRIDGE_CREDIT_SCALE;
	uint64_t byte_max = MASIL_BRIDGE_SNAPSHOT_BURST *
	    MASIL_BRIDGE_CREDIT_SCALE;
	uint64_t request_cost = MASIL_BRIDGE_CREDIT_SCALE;
	uint64_t byte_cost = estimated_bytes * MASIL_BRIDGE_CREDIT_SCALE;

	now = masil_bridge_now_usec();
	elapsed = now - masil_bridge_snapshot_updated;
	masil_bridge_snapshot_updated = now;
	if (elapsed >= 2000000) {
		masil_bridge_snapshot_credit = request_max;
		masil_bridge_snapshot_byte_credit = byte_max;
	} else {
		add = elapsed * MASIL_BRIDGE_SNAPSHOT_RATE;
		if (add >= request_max - masil_bridge_snapshot_credit)
			masil_bridge_snapshot_credit = request_max;
		else
			masil_bridge_snapshot_credit += add;
		add = elapsed * MASIL_BRIDGE_SNAPSHOT_BYTES;
		if (add >= byte_max - masil_bridge_snapshot_byte_credit)
			masil_bridge_snapshot_byte_credit = byte_max;
		else
			masil_bridge_snapshot_byte_credit += add;
	}
	if (masil_bridge_snapshot_credit < request_cost ||
	    masil_bridge_snapshot_byte_credit < byte_cost) {
		masil_bridge_stats.rate_limited++;
		return (-1);
	}
	masil_bridge_snapshot_credit -= request_cost;
	masil_bridge_snapshot_byte_credit -= byte_cost;
	return (0);
}

static int
masil_bridge_snapshot_adjust(size_t estimated_bytes, size_t actual_bytes)
{
	uint64_t byte_max = MASIL_BRIDGE_SNAPSHOT_BURST *
	    MASIL_BRIDGE_CREDIT_SCALE;
	uint64_t difference;

	if (actual_bytes <= estimated_bytes) {
		difference = (estimated_bytes - actual_bytes) *
		    MASIL_BRIDGE_CREDIT_SCALE;
		if (difference >= byte_max - masil_bridge_snapshot_byte_credit)
			masil_bridge_snapshot_byte_credit = byte_max;
		else
			masil_bridge_snapshot_byte_credit += difference;
		return (0);
	}
	difference = (actual_bytes - estimated_bytes) *
	    MASIL_BRIDGE_CREDIT_SCALE;
	if (masil_bridge_snapshot_byte_credit < difference) {
		masil_bridge_stats.rate_limited++;
		return (-1);
	}
	masil_bridge_snapshot_byte_credit -= difference;
	return (0);
}

static int
masil_bridge_snapshot(struct masil_bridge_client *client,
	const char *request_id, size_t request_id_length, yyjson_val *root)
{
	struct masil_json_builder builder = {
	    masil_bridge_response, 0, sizeof masil_bridge_response, 0
	};
	struct window_pane	*wp;
	struct screen		*screen;
	struct grid_cell	 cell;
	yyjson_val		*value;
	char			 text[MASIL_BRIDGE_TEXT];
	uint64_t		 generation, pty_generation, expected;
	size_t		 text_length = 0, encoded_length = 0;
	size_t		 clipped_bytes = 0, cell_encoded;
	u_int		 pane_id, width, height, columns, rows, first, x, y;
	size_t		 estimated_bytes;
	int		 accepting = 1;

	value = yyjson_obj_get(root, "pane_id");
	if (masil_bridge_pane_id(value, &pane_id) != 0)
		return (masil_bridge_error(client, request_id, request_id_length,
		    "invalid_pane_id", "pane_id must have the form %N"));
	wp = window_pane_find_by_id(pane_id);
	if (wp == NULL)
		return (masil_bridge_error(client, request_id, request_id_length,
		    "target_gone", "pane does not exist"));
	if (wp->masil_generation_exhausted)
		return (masil_bridge_error(client, request_id, request_id_length,
		    "generation_exhausted", "pane generations cannot be reused"));
	value = yyjson_obj_get(root, "expected_core_boot_id");
	if (value != NULL && (!yyjson_is_str(value) ||
	    yyjson_get_len(value) != strlen(masil_bridge_boot_id) ||
	    memcmp(yyjson_get_str(value), masil_bridge_boot_id,
	    yyjson_get_len(value)) != 0))
		return (masil_bridge_error(client, request_id, request_id_length,
		    "boot_mismatch", "core boot id does not match"));
	value = yyjson_obj_get(root, "expected_pty_generation");
	if (value != NULL && (masil_bridge_parse_decimal(value, &expected) != 0 ||
	    expected != wp->masil_pty_generation))
		return (masil_bridge_error(client, request_id, request_id_length,
		    "pty_generation_mismatch", "pane PTY generation does not match"));
	value = yyjson_obj_get(root, "expected_screen_generation");
	if (value != NULL && (masil_bridge_parse_decimal(value, &expected) != 0 ||
	    expected != wp->masil_screen_generation))
		return (masil_bridge_error(client, request_id, request_id_length,
		    "screen_generation_mismatch",
		    "pane screen generation does not match"));
	pty_generation = wp->masil_pty_generation;
	generation = wp->masil_screen_generation;
	screen = &wp->base;
	width = screen_size_x(screen);
	height = screen_size_y(screen);
	columns = width > MASIL_BRIDGE_COLUMNS ? MASIL_BRIDGE_COLUMNS : width;
	rows = height > MASIL_BRIDGE_ROWS ? MASIL_BRIDGE_ROWS : height;
	first = screen_hsize(screen) + height - rows;
	estimated_bytes = (size_t)rows * columns * 4;
	if (estimated_bytes > MASIL_BRIDGE_TEXT)
		estimated_bytes = MASIL_BRIDGE_TEXT;
	if (masil_bridge_snapshot_admit(estimated_bytes) != 0)
		return (masil_bridge_error(client, request_id, request_id_length,
		    "rate_limited", "snapshot budget is temporarily exhausted"));

	for (y = 0; y < rows; y++) {
		for (x = 0; x < columns; x++) {
			grid_get_cell(screen->grid, x, first + y, &cell);
			if (cell.flags & GRID_FLAG_PADDING)
				continue;
			cell_encoded = masil_json_quoted_length(
			    (const char *)cell.data.data, cell.data.size);
			if (accepting &&
			    text_length + cell.data.size <= sizeof text &&
			    encoded_length + cell_encoded <=
			    MASIL_BRIDGE_TEXT_ENCODED) {
				memcpy(text + text_length, cell.data.data,
				    cell.data.size);
				text_length += cell.data.size;
				encoded_length += cell_encoded;
			} else {
				accepting = 0;
				clipped_bytes += cell.data.size;
			}
		}
		if (y + 1 < rows) {
			if (accepting && text_length + 1 <= sizeof text &&
			    encoded_length + 2 <= MASIL_BRIDGE_TEXT_ENCODED) {
				text[text_length++] = '\n';
				encoded_length += 2;
			} else {
				accepting = 0;
				clipped_bytes++;
			}
		}
	}
	if (masil_bridge_snapshot_adjust(estimated_bytes, text_length) != 0)
		return (masil_bridge_error(client, request_id, request_id_length,
		    "rate_limited", "snapshot byte budget is temporarily exhausted"));

	masil_json_puts(&builder,
	    "{\"v\":1,\"kind\":\"snapshot\",\"request_id\":");
	masil_json_quote(&builder, request_id, request_id_length);
	masil_json_printf(&builder,
	    ",\"core_boot_id\":\"%s\",\"pane_id\":\"%%%u\","
	    "\"pty_generation\":\"%llu\",\"screen_generation\":\"%llu\","
	    "\"width\":%u,\"height\":%u,\"cursor\":{\"x\":%u,\"y\":%u},"
	    "\"screen_kind\":\"%s\",\"source\":{\"start_row\":%u,"
	    "\"rows\":%u,\"columns\":%u},\"clipped\":{\"rows\":%u,"
	    "\"columns\":%u,\"bytes\":%zu},\"complete\":%s,\"text\":",
	    masil_bridge_boot_id, wp->id,
	    (unsigned long long)pty_generation,
	    (unsigned long long)generation, width, height, screen->cx, screen->cy,
	    SCREEN_IS_ALTERNATE(screen) ? "alternate" : "main", height - rows,
	    rows, columns, height - rows, width - columns, clipped_bytes,
	    generation == wp->masil_screen_generation &&
	    pty_generation == wp->masil_pty_generation && clipped_bytes == 0 &&
	    rows == height && columns == width ?
	    "true" : "false");
	masil_json_quote(&builder, text, text_length);
	masil_json_puts(&builder, "}");
	masil_bridge_stats.snapshots++;
	return (masil_bridge_queue_builder(client, &builder));
}

static int
masil_bridge_stats_response(struct masil_bridge_client *client,
	const char *request_id, size_t request_id_length)
{
	struct masil_json_builder builder = {
	    masil_bridge_response, 0, sizeof masil_bridge_response, 0
	};

	masil_json_puts(&builder,
	    "{\"v\":1,\"kind\":\"stats\",\"request_id\":");
	masil_json_quote(&builder, request_id, request_id_length);
	masil_json_printf(&builder,
	    ",\"core_boot_id\":\"%s\",\"revision\":\"%llu\","
	    "\"revision_exhausted\":%s,\"clients\":%zu,"
	    "\"tx_queued_bytes\":%zu,\"watched_panes\":%zu,"
	    "\"watch_subscriptions\":%zu,\"journal_events\":%zu,"
	    "\"pending_dirty\":%zu,\"flush_timer_active\":%s,"
	    "\"counters\":{"
	    "\"connects\":\"%llu\",\"disconnects\":\"%llu\","
	    "\"rx_frames\":\"%llu\",\"tx_frames\":\"%llu\","
	    "\"rx_bytes\":\"%llu\",\"tx_bytes\":\"%llu\","
	    "\"requests\":\"%llu\",\"rejected\":\"%llu\","
	    "\"protocol_errors\":\"%llu\","
	    "\"inventory_requests\":\"%llu\",\"snapshots\":\"%llu\","
	    "\"rate_limited\":\"%llu\",\"events\":\"%llu\","
	    "\"coalesced\":\"%llu\",\"gaps\":\"%llu\"}}",
	    masil_bridge_boot_id, (unsigned long long)masil_bridge_revision,
	    masil_bridge_revision_exhausted ? "true" : "false",
	    masil_bridge_client_count, masil_bridge_tx_bytes,
	    masil_bridge_used_slot_count, masil_bridge_watch_clients,
	    masil_bridge_journal_count, masil_bridge_pending_dirty,
	    masil_bridge_dirty_timer_active ? "true" : "false",
	    (unsigned long long)masil_bridge_stats.connects,
	    (unsigned long long)masil_bridge_stats.disconnects,
	    (unsigned long long)masil_bridge_stats.rx_frames,
	    (unsigned long long)masil_bridge_stats.tx_frames,
	    (unsigned long long)masil_bridge_stats.rx_bytes,
	    (unsigned long long)masil_bridge_stats.tx_bytes,
	    (unsigned long long)masil_bridge_stats.requests,
	    (unsigned long long)masil_bridge_stats.rejected,
	    (unsigned long long)masil_bridge_stats.protocol_errors,
	    (unsigned long long)masil_bridge_stats.inventory_requests,
	    (unsigned long long)masil_bridge_stats.snapshots,
	    (unsigned long long)masil_bridge_stats.rate_limited,
	    (unsigned long long)masil_bridge_stats.events,
	    (unsigned long long)masil_bridge_stats.coalesced,
	    (unsigned long long)masil_bridge_stats.gaps);
	return (masil_bridge_queue_builder(client, &builder));
}

static int
masil_bridge_process(struct masil_bridge_client *client, u_char *payload,
    size_t length)
{
	static const char *const hello_fields[] = {
	    "v", "kind", "request_id", "role"
	};
	static const char *const inventory_fields[] = {
	    "v", "kind", "request_id", "cursor", "revision"
	};
	static const char *const snapshot_fields[] = {
	    "v", "kind", "request_id", "pane_id", "expected_core_boot_id",
	    "expected_pty_generation", "expected_screen_generation"
	};
	static const char *const stats_fields[] = {
	    "v", "kind", "request_id"
	};
	static const char *const watch_fields[] = {
	    "v", "kind", "request_id", "expected_core_boot_id", "pane_ids",
	    "lifecycle", "clients"
	};
	yyjson_alc	 allocator;
	yyjson_doc	*document;
	yyjson_val	*root, *version, *kind, *request;
	const char	*code = NULL, *request_id = NULL;
	size_t		 request_id_length = 0;
	int		 action_result, result;

	if (!yyjson_alc_pool_init(&allocator, masil_bridge_codec_pool,
	    sizeof masil_bridge_codec_pool))
		return (-1);
	document = yyjson_read_opts((char *)payload, length, YYJSON_READ_NOFLAG,
	    &allocator, NULL);
	if (document == NULL) {
		masil_bridge_stats.protocol_errors++;
		return (masil_bridge_error(client, NULL, 0, "invalid_json",
		    "payload is not strict UTF-8 JSON"));
	}
	root = yyjson_doc_get_root(document);
	request = yyjson_obj_get(root, "request_id");
	if (yyjson_is_str(request) && yyjson_get_len(request) > 0 &&
	    yyjson_get_len(request) <= 128) {
		request_id = yyjson_get_str(request);
		request_id_length = yyjson_get_len(request);
	}
	version = yyjson_obj_get(root, "v");
	kind = yyjson_obj_get(root, "kind");
	if (request_id == NULL || !yyjson_is_uint(version) ||
	    yyjson_get_uint(version) != 1 || !yyjson_is_str(kind)) {
		result = masil_bridge_error(client, request_id, request_id_length,
		    "invalid_request", "v, kind, and request_id are required");
		goto out;
	}
	if (masil_bridge_exiting &&
	    (yyjson_equals_str(kind, "hello") || yyjson_equals_str(kind, "watch"))) {
		client->close_after_write = 1;
		result = masil_bridge_error(client, request_id, request_id_length,
		    "server_exiting", "server is exiting");
		goto out;
	}

	if (!client->hello_done && !yyjson_equals_str(kind, "hello")) {
		result = masil_bridge_error(client, request_id, request_id_length,
		    "handshake_required", "hello must be the first request");
		goto out;
	}
	if (yyjson_equals_str(kind, "hello")) {
		if (client->hello_done) {
			result = masil_bridge_error(client, request_id,
			    request_id_length, "already_initialized",
			    "hello was already completed");
			goto out;
		}
		if (masil_bridge_schema(root, hello_fields,
		    nitems(hello_fields), &code) != 0) {
			result = masil_bridge_error(client, request_id,
			    request_id_length, code, "hello schema rejected");
			goto out;
		}
		result = masil_bridge_hello(client, request_id, request_id_length,
		    root);
	} else if (yyjson_equals_str(kind, "guarded_action") ||
	    yyjson_equals_str(kind, "retire_receipt") ||
	    yyjson_equals_str(kind, "ledger_query") ||
	    yyjson_equals_str(kind, "ledger_list") ||
	    yyjson_equals_str(kind, "ledger_epochs")) {
		/* masil: only the epoch-owning coordinator may access the ledger. */
		if (!client->coordinator) {
			result = masil_bridge_error(client, request_id, request_id_length,
			    "coordinator_required",
			    "this request requires the coordinator connection");
		} else if (client->watching) {
			result = masil_bridge_error(client, request_id, request_id_length,
			    "stream_active", "watch connection does not accept requests");
			client->close_after_write = 1;
		} else if (!masil_bridge_coordinator_response_ready(client)) {
			/*
			 * Non-coordinator queues reserve one complete coordinator frame, so an
			 * admitted action response cannot fail after its effect. Filling the
			 * pre-existing queue enough to reach bridge_busy needs stalled peers;
			 * that transport race is intentionally impractical for the ledger test.
			 */
			result = masil_bridge_error(client, request_id, request_id_length,
			    "bridge_busy", "coordinator response capacity is unavailable");
		} else {
			action_result = masil_action_dispatch(client->action_epoch,
			    request_id, request_id_length, yyjson_get_str(kind), root,
			    masil_bridge_response, sizeof masil_bridge_response);
			if (action_result < 0)
				result = -1;
			else {
				if (action_result > 0)
					masil_bridge_stats.rejected++;
				result = masil_bridge_queue(client, masil_bridge_response,
				    strlen(masil_bridge_response));
			}
		}
	} else if (client->watching) {
		result = masil_bridge_error(client, request_id, request_id_length,
		    "stream_active", "watch connection does not accept requests");
		client->close_after_write = 1;
	} else if (yyjson_equals_str(kind, "inventory")) {
		if (masil_bridge_schema(root, inventory_fields,
		    nitems(inventory_fields), &code) != 0) {
			result = masil_bridge_error(client, request_id,
			    request_id_length, code, "inventory schema rejected");
			goto out;
		}
		result = masil_bridge_inventory(client, request_id,
		    request_id_length, root);
	} else if (yyjson_equals_str(kind, "snapshot")) {
		if (masil_bridge_schema(root, snapshot_fields,
		    nitems(snapshot_fields), &code) != 0) {
			result = masil_bridge_error(client, request_id,
			    request_id_length, code, "snapshot schema rejected");
			goto out;
		}
		result = masil_bridge_snapshot(client, request_id,
		    request_id_length, root);
	} else if (yyjson_equals_str(kind, "watch")) {
		if (masil_bridge_schema(root, watch_fields,
		    nitems(watch_fields), &code) != 0) {
			result = masil_bridge_error(client, request_id,
			    request_id_length, code, "watch schema rejected");
			goto out;
		}
		result = masil_bridge_watch(client, request_id,
		    request_id_length, root);
	} else if (yyjson_equals_str(kind, "stats")) {
		if (masil_bridge_schema(root, stats_fields,
		    nitems(stats_fields), &code) != 0) {
			result = masil_bridge_error(client, request_id,
			    request_id_length, code, "stats schema rejected");
			goto out;
		}
		result = masil_bridge_stats_response(client, request_id,
		    request_id_length);
	} else
		result = masil_bridge_error(client, request_id, request_id_length,
		    "unsupported_kind", "request kind is not supported");

out:
	masil_bridge_stats.requests++;
	yyjson_doc_free(document);
	return (result);
}

static int
masil_bridge_consume(struct masil_bridge_client *client, u_int *frames,
    size_t *bytes, uint64_t started)
{
	uint32_t length;
	size_t consumed;

	while (client->tx == NULL && client->rx_len >= 4) {
		length = masil_bridge_get_u32(client->rx);
		if (length == 0 || length > MASIL_BRIDGE_RX_MAX) {
			masil_bridge_stats.protocol_errors++;
			if (masil_bridge_error(client, NULL, 0, "invalid_frame_length",
			    "frame length is outside the allowed range") != 0)
				return (-1);
			client->close_after_write = 1;
			return (0);
		}
		if (client->rx_len < (size_t)length + 4)
			return (0);
		consumed = (size_t)length + 4;
		masil_bridge_stats.rx_frames++;
		if (masil_bridge_process(client, client->rx + 4, length) != 0)
			return (-1);
		client->rx_len -= consumed;
		if (client->rx_len != 0)
			memmove(client->rx, client->rx + consumed, client->rx_len);
		(*frames)++;
		*bytes += consumed;
		if (*frames >= MASIL_BRIDGE_BATCH_FRAMES ||
		    *bytes >= MASIL_BRIDGE_BATCH_BYTES ||
		    masil_bridge_now_usec() - started >= MASIL_BRIDGE_BATCH_USEC)
			return (0);
	}
	return (0);
}

static void
masil_bridge_client_callback(int fd, short events, void *data)
{
	struct masil_bridge_client *client = data;
	ssize_t			 used;
	size_t			 available, bytes = 0;
	u_int			 frames = 0;
	uint64_t		 started = masil_bridge_now_usec();
	int			 flags = 0;

	if ((events & EV_WRITE) && client->tx != NULL) {
#ifdef MSG_NOSIGNAL
		flags = MSG_NOSIGNAL;
#endif
		used = send(fd, client->tx + client->tx_off,
		    client->tx_len - client->tx_off, flags);
		if (used == -1 && errno != EAGAIN && errno != EINTR) {
			masil_bridge_client_close(client);
			return;
		}
		if (used > 0) {
			client->tx_off += used;
			masil_bridge_stats.tx_bytes += used;
		}
		if (client->tx_off == client->tx_len) {
			masil_bridge_tx_bytes -= client->tx_len;
			free(client->tx);
			client->tx = NULL;
			client->tx_len = client->tx_off = 0;
			if (client->close_after_write) {
				masil_bridge_client_close(client);
				return;
			}
		}
	}

	if (client->tx == NULL && masil_bridge_consume(client, &frames, &bytes,
	    started) != 0) {
		masil_bridge_client_close(client);
		return;
	}
	if (client->tx == NULL && client->watching &&
	    masil_bridge_watch_pump(client) != 0) {
		masil_bridge_client_close(client);
		return;
	}
	if ((events & EV_READ) && client->tx == NULL &&
	    frames < MASIL_BRIDGE_BATCH_FRAMES &&
	    masil_bridge_now_usec() - started < MASIL_BRIDGE_BATCH_USEC) {
		available = sizeof client->rx - client->rx_len;
		used = recv(fd, client->rx + client->rx_len, available, 0);
		if (used == 0) {
			if (client->rx_len != 0)
				masil_bridge_stats.protocol_errors++;
			masil_bridge_client_close(client);
			return;
		}
		if (used == -1 && errno != EAGAIN && errno != EINTR) {
			masil_bridge_client_close(client);
			return;
		}
		if (used > 0) {
			client->rx_len += used;
			masil_bridge_stats.rx_bytes += used;
			bytes += used;
			if (masil_bridge_consume(client, &frames, &bytes,
			    started) != 0) {
				masil_bridge_client_close(client);
				return;
			}
		}
	}
	masil_bridge_client_deadline(client);
	masil_bridge_client_update(client);
	if (client->watch_reschedule && client->tx == NULL) {
		client->watch_reschedule = 0;
		event_active(&client->event, EV_READ, 1);
	}
}

static void
masil_bridge_client_update(struct masil_bridge_client *client)
{
	short events;

	events = client->tx == NULL ? EV_READ : EV_WRITE;
	if (event_initialized(&client->event))
		event_del(&client->event);
	event_set(&client->event, client->fd, events, masil_bridge_client_callback,
	    client);
	event_add(&client->event, NULL);
}

/* masil: stop may run before libevent dispatches the terminal watch event. */
static void
masil_bridge_client_flush(struct masil_bridge_client *client)
{
	ssize_t	used;
	int	flags = 0;

#ifdef MSG_NOSIGNAL
	flags = MSG_NOSIGNAL;
#endif
	while (client->tx != NULL) {
		used = send(client->fd, client->tx + client->tx_off,
		    client->tx_len - client->tx_off, flags);
		if (used == -1) {
			if (errno == EINTR)
				continue;
			/* Nonblocking shutdown writes are intentionally best effort. */
			return;
		}
		if (used == 0)
			return;
		client->tx_off += used;
		masil_bridge_stats.tx_bytes += used;
		if (client->tx_off == client->tx_len) {
			masil_bridge_tx_bytes -= client->tx_len;
			free(client->tx);
			client->tx = NULL;
			client->tx_len = client->tx_off = 0;
		}
	}
}

/*
 * Stop runs outside the event loop. Drain an existing frame and then pump
 * each queued visible event in turn, so the terminal server_exiting record
 * can catch up to a lagging subscriber. The journal bound keeps this
 * best-effort path finite; a nonblocking write that remains queued stops it.
 */
static void
masil_bridge_client_stop_flush(struct masil_bridge_client *client)
{
	size_t	 attempts;
	uint64_t cursor;
	int	 had_tx;

	for (attempts = 0; attempts < MASIL_BRIDGE_STOP_FLUSHES; attempts++) {
		had_tx = client->tx != NULL;
		cursor = client->watch_cursor;
		if (!had_tx && masil_bridge_exiting && client->watching &&
		    masil_bridge_watch_pump(client) != 0)
			return;
		masil_bridge_client_flush(client);
		/* A terminal frame (gap, error) ends the stream; send nothing after. */
		if (client->tx != NULL || client->close_after_write ||
		    (!had_tx && client->watch_cursor == cursor))
			return;
	}
}

static void
masil_bridge_accept(__unused int fd, short events, __unused void *data)
{
	struct sockaddr_un	 address;
	struct masil_bridge_client *client;
	socklen_t		 length = sizeof address;
	uid_t			 uid;
	gid_t			 gid;
	int			 client_fd;

	if (!(events & EV_READ))
		return;
	client_fd = accept(masil_bridge_fd, (struct sockaddr *)&address, &length);
	if (client_fd == -1)
		return;
	if (fcntl(client_fd, F_SETFD, FD_CLOEXEC) == -1) {
		close(client_fd);
		return;
	}
	/*
	 * masil: ordinary clients and pending hellos share 30 slots. While no
	 * coordinator is connected, accept one extra pending hello but require
	 * that connection to claim the coordinator role. The fourth pending hello
	 * is reserved the same way, so three idle hellos cannot consume it. An
	 * idle socket holds a reservation at most until the pre-hello timeout.
	 */
	if (masil_bridge_client_count >= MASIL_BRIDGE_CLIENTS ||
	    masil_bridge_unauthenticated >= MASIL_BRIDGE_UNAUTHENTICATED ||
	    masil_bridge_ordinary_clients + masil_bridge_unauthenticated >=
	    MASIL_BRIDGE_ORDINARY_CLIENTS +
	    (masil_bridge_coordinator_connected ? 0 : 1) ||
	    getpeereid(client_fd, &uid, &gid) != 0 || uid != geteuid()) {
		close(client_fd);
		return;
	}
	setblocking(client_fd, 0);
#ifdef SO_NOSIGPIPE
	{
		int one = 1;
		setsockopt(client_fd, SOL_SOCKET, SO_NOSIGPIPE, &one, sizeof one);
	}
#endif
	client = xcalloc(1, sizeof *client);
	client->fd = client_fd;
	if (!masil_bridge_coordinator_connected &&
	    (masil_bridge_ordinary_clients + masil_bridge_unauthenticated >=
	    MASIL_BRIDGE_ORDINARY_CLIENTS ||
	    masil_bridge_unauthenticated >= MASIL_BRIDGE_UNAUTHENTICATED - 1))
		client->coordinator_only = 1;
	client->next = masil_bridge_clients;
	masil_bridge_clients = client;
	masil_bridge_client_count++;
	masil_bridge_unauthenticated++;
	masil_bridge_stats.connects++;
	masil_bridge_client_update(client);
	masil_bridge_client_deadline(client);
}

static int
masil_bridge_private_parent(const char *path)
{
	char	 directory[sizeof masil_bridge_path];
	char	*slash;
	struct stat sb;
	size_t	 length;

	length = strlcpy(directory, path, sizeof directory);
	if (length >= sizeof directory)
		return (-1);
	slash = strrchr(directory, '/');
	if (slash == NULL || slash == directory || slash[1] == '\0')
		return (-1);
	*slash = '\0';
	if (stat(directory, &sb) != 0 || !S_ISDIR(sb.st_mode) ||
	    sb.st_uid != geteuid() || (sb.st_mode & (S_IRWXG|S_IRWXO)) != 0)
		return (-1);
	return (0);
}

/* masil: derive a private default beside a safe server socket directory. */
static int
masil_bridge_default_path(char *path, size_t path_size, char *cause,
    size_t cause_size)
{
	char		 absolute[PATH_MAX], parent_input[PATH_MAX];
	char		 parent[PATH_MAX], directory[PATH_MAX], cwd[PATH_MAX];
	const char	*base;
	char		*slash;
	struct stat	 sb;
	mode_t		 old_mask;
	int		 length, saved_errno;

	if (socket_path == NULL || *socket_path == '\0') {
		xsnprintf(cause, cause_size, "the server socket path is empty");
		return (-1);
	}
	if (*socket_path == '/') {
		if (strlcpy(absolute, socket_path, sizeof absolute) >=
		    sizeof absolute) {
			xsnprintf(cause, cause_size,
			    "the server socket path is too long");
			return (-1);
		}
	} else {
		if (getcwd(cwd, sizeof cwd) == NULL) {
			xsnprintf(cause, cause_size,
			    "the server working directory is unavailable: %s",
			    strerror(errno));
			return (-1);
		}
		length = snprintf(absolute, sizeof absolute, "%s/%s", cwd,
		    socket_path);
		if (length < 0 || (size_t)length >= sizeof absolute) {
			xsnprintf(cause, cause_size,
			    "the absolute server socket path is too long");
			return (-1);
		}
	}
	slash = strrchr(absolute, '/');
	if (slash == NULL || slash[1] == '\0') {
		xsnprintf(cause, cause_size,
		    "the server socket path has no basename");
		return (-1);
	}
	base = slash + 1;
	if (slash == absolute)
		strlcpy(parent_input, "/", sizeof parent_input);
	else {
		length = snprintf(parent_input, sizeof parent_input, "%.*s",
		    (int)(slash - absolute), absolute);
		if (length < 0 || (size_t)length >= sizeof parent_input) {
			xsnprintf(cause, cause_size,
			    "the server socket parent path is too long");
			return (-1);
		}
	}
	if (realpath(parent_input, parent) == NULL) {
		xsnprintf(cause, cause_size,
		    "cannot resolve server socket parent %s: %s", parent_input,
		    strerror(errno));
		return (-1);
	}
	if (stat(parent, &sb) != 0 || !S_ISDIR(sb.st_mode)) {
		xsnprintf(cause, cause_size,
		    "server socket parent %s is not a directory", parent);
		return (-1);
	}
	if (sb.st_uid != geteuid()) {
		xsnprintf(cause, cause_size,
		    "server socket parent %s is not owned by uid %lld", parent,
		    (long long)geteuid());
		return (-1);
	}
	if (sb.st_mode & (S_IWGRP|S_IWOTH)) {
		xsnprintf(cause, cause_size,
		    "server socket parent %s is group or other writable", parent);
		return (-1);
	}
	length = snprintf(directory, sizeof directory, "%s%s.masil", parent,
	    strcmp(parent, "/") == 0 ? "" : "/");
	if (length < 0 || (size_t)length >= sizeof directory) {
		xsnprintf(cause, cause_size,
		    "the default bridge directory path is too long");
		return (-1);
	}
	length = snprintf(path, path_size, "%s/%s.bridge", directory, base);
	if (length < 0 || (size_t)length >= path_size) {
		xsnprintf(cause, cause_size,
		    "the default bridge socket path is too long");
		return (-1);
	}

	old_mask = umask(0);
	if (mkdir(directory, S_IRWXU) != 0 && errno != EEXIST) {
		saved_errno = errno;
		umask(old_mask);
		errno = saved_errno;
		xsnprintf(cause, cause_size,
		    "cannot create private bridge directory %s: %s", directory,
		    strerror(errno));
		return (-1);
	}
	umask(old_mask);
	if (lstat(directory, &sb) != 0 || !S_ISDIR(sb.st_mode) ||
	    sb.st_uid != geteuid() || (sb.st_mode & 07777) != S_IRWXU) {
		xsnprintf(cause, cause_size,
		    "bridge directory %s is not an owner-only 0700 directory",
		    directory);
		return (-1);
	}
	return (0);
}

/* masil: reclaim only a dead owner socket in the derived 0700 directory. */
static int
masil_bridge_reclaim_stale(const char *path)
{
	struct sockaddr_un	 address;
	struct stat		 sb, current;
	int			 fd, error;

	if (lstat(path, &sb) != 0)
		return (errno == ENOENT ? 0 : -1);
	if (!S_ISSOCK(sb.st_mode) || sb.st_uid != geteuid())
		return (-1);
	fd = socket(AF_UNIX, SOCK_STREAM, 0);
	if (fd == -1)
		return (-1);
	setblocking(fd, 0);
	memset(&address, 0, sizeof address);
	address.sun_family = AF_UNIX;
	if (strlcpy(address.sun_path, path, sizeof address.sun_path) >=
	    sizeof address.sun_path) {
		close(fd);
		errno = ENAMETOOLONG;
		return (-1);
	}
	if (connect(fd, (struct sockaddr *)&address, sizeof address) == 0) {
		close(fd);
		errno = EADDRINUSE;
		return (-1);
	}
	error = errno;
	close(fd);
	if (error != ECONNREFUSED && error != ENOENT) {
		errno = error;
		return (-1);
	}
	if (lstat(path, &current) != 0) {
		if (errno == ENOENT)
			return (0);
		return (-1);
	}
	/* masil: never unlink a path that changed after the failed probe. */
	if (!S_ISSOCK(current.st_mode) || current.st_uid != geteuid() ||
	    current.st_dev != sb.st_dev || current.st_ino != sb.st_ino) {
		errno = EPERM;
		return (-1);
	}
	return (unlink(path));
}

const char *
masil_bridge_get_boot_id(void)
{
	if (*masil_bridge_boot_id == '\0')
		masil_bridge_new_boot_id();
	return (masil_bridge_boot_id);
}

static int
masil_bridge_listener_live(void)
{
	struct stat sb;

	if (!masil_bridge_enabled || masil_bridge_fd == -1 ||
	    masil_bridge_path[0] == '\0')
		return (0);
	return (lstat(masil_bridge_path, &sb) == 0 &&
	    sb.st_dev == masil_bridge_socket_dev &&
	    sb.st_ino == masil_bridge_socket_ino);
}

/* masil: expose only the active bridge endpoint to format expansion. */
const char *
masil_bridge_get_socket_path(void)
{
	if (!masil_bridge_listener_live())
		return ("");
	return (masil_bridge_path);
}

/*
 * masil: formats can detect C1a without opening a coordinator connection.
 * The answer is fixed for the boot (bridge started and actions built in), so
 * a lost socket makes effects fail rather than switch a caller to the native
 * path; masil_bridge_socket shows whether the listener is live.
 */
int
masil_bridge_actions_supported(void)
{
	return (masil_bridge_enabled && masil_action_supported());
}

/* masil: client ids are stable only within the current core boot. */
char *
masil_bridge_client_id(struct client *c)
{
	char	*value;

	xasprintf(&value, "%s:%llu", masil_bridge_get_boot_id(),
	    (unsigned long long)c->masil_serial);
	return (value);
}

/* masil: small in-core FNV-1a fallback for opaque client profiles. */
static uint64_t
masil_bridge_fnv1a(uint64_t hash, const void *data, size_t length)
{
	const u_char	*bytes = data;
	size_t		 i;

	for (i = 0; i < length; i++) {
		hash ^= bytes[i];
		hash *= 1099511628211ULL;
	}
	return (hash);
}

char *
masil_bridge_client_profile(struct client *c)
{
	static const char *const names[] = {
	    "TERM_SESSION_ID", "ITERM_SESSION_ID", "WT_SESSION",
	    "WEZTERM_PANE", "KITTY_WINDOW_ID"
	};
	struct environ_entry	*entry, *term;
	const char		*value = NULL, *term_value = "";
	char			 uid[32], *result;
	uid_t			 peer_uid;
	uint64_t		 hash = 14695981039346656037ULL;
	size_t			 i;
	static const u_char	 separator = '\0';

	entry = environ_find(c->environ, "MASIL_CLIENT_KEY");
	if (entry != NULL && entry->value != NULL && *entry->value != '\0') {
		hash = masil_bridge_fnv1a(hash, entry->value,
		    strlen(entry->value));
		goto done;
	}
	for (i = 0; i < nitems(names); i++) {
		entry = environ_find(c->environ, names[i]);
		if (entry != NULL && entry->value != NULL) {
			value = entry->value;
			break;
		}
	}
	if (value == NULL)
		return (xstrdup("unstable"));
	peer_uid = proc_get_peer_uid(c->peer);
	snprintf(uid, sizeof uid, "%lld", (long long)peer_uid);
	term = environ_find(c->environ, "TERM");
	if (term != NULL && term->value != NULL)
		term_value = term->value;
	hash = masil_bridge_fnv1a(hash, value, strlen(value));
	hash = masil_bridge_fnv1a(hash, &separator, sizeof separator);
	hash = masil_bridge_fnv1a(hash, uid, strlen(uid));
	hash = masil_bridge_fnv1a(hash, &separator, sizeof separator);
	hash = masil_bridge_fnv1a(hash, term_value, strlen(term_value));

done:
	xasprintf(&result, "%016llx", (unsigned long long)hash);
	return (result);
}

void
masil_bridge_start(void)
{
	const char		*configured = getenv("MASIL_BRIDGE_SOCKET");
	const char		*path;
	char			 cause[256];
	struct sockaddr_un	 address;
	struct stat		 sb;
	mode_t			 old_mask;
	int			 derived = 0, saved_errno;

	(void)masil_bridge_get_boot_id();
	if (masil_bridge_enabled)
		return;
	masil_bridge_path_derived = 0;
	/* masil: unset enables the private default; empty and off disable it. */
	if (configured != NULL &&
	    (*configured == '\0' || strcmp(configured, "off") == 0))
		return;
	if (configured == NULL) {
		derived = 1;
		if (masil_bridge_default_path(masil_bridge_path,
		    sizeof masil_bridge_path, cause, sizeof cause) != 0) {
			server_add_message("masil bridge is off: %s", cause);
			masil_bridge_path[0] = '\0';
			return;
		}
		path = masil_bridge_path;
	} else
		path = configured;
	if ((!derived && masil_bridge_private_parent(path) != 0) ||
	    (!derived && strlcpy(masil_bridge_path, path,
	    sizeof masil_bridge_path) >= sizeof masil_bridge_path)) {
		log_debug("masil bridge: socket path must be in a private owner directory");
		masil_bridge_path[0] = '\0';
		return;
	}
	masil_bridge_path_derived = derived;
	if ((derived && masil_bridge_reclaim_stale(masil_bridge_path) != 0) ||
	    (!derived && (lstat(masil_bridge_path, &sb) == 0 ||
	    errno != ENOENT))) {
		log_debug("masil bridge: refusing existing socket path %s",
		    masil_bridge_path);
		masil_bridge_path_derived = 0;
		masil_bridge_path[0] = '\0';
		return;
	}
	masil_bridge_fd = socket(AF_UNIX, SOCK_STREAM, 0);
	if (masil_bridge_fd == -1)
		goto fail;
	if (fcntl(masil_bridge_fd, F_SETFD, FD_CLOEXEC) == -1)
		goto fail;
	memset(&address, 0, sizeof address);
	address.sun_family = AF_UNIX;
	strlcpy(address.sun_path, masil_bridge_path, sizeof address.sun_path);
	old_mask = umask(S_IRWXG|S_IRWXO);
	if (bind(masil_bridge_fd, (struct sockaddr *)&address, sizeof address) != 0) {
		saved_errno = errno;
		umask(old_mask);
		errno = saved_errno;
		goto fail;
	}
	umask(old_mask);
	/* masil: capture the bound inode before any cleanup-capable failure. */
	if (lstat(masil_bridge_path, &sb) != 0)
		goto fail_bound;
	masil_bridge_socket_dev = sb.st_dev;
	masil_bridge_socket_ino = sb.st_ino;
	if (chmod(masil_bridge_path, S_IRUSR|S_IWUSR) != 0 ||
	    listen(masil_bridge_fd, MASIL_BRIDGE_CLIENTS) != 0)
		goto fail_bound;
	setblocking(masil_bridge_fd, 0);
	masil_bridge_snapshot_updated = masil_bridge_now_usec();
	masil_bridge_snapshot_credit = MASIL_BRIDGE_SNAPSHOT_RATE *
	    MASIL_BRIDGE_CREDIT_SCALE;
	masil_bridge_snapshot_byte_credit = MASIL_BRIDGE_SNAPSHOT_BURST *
	    MASIL_BRIDGE_CREDIT_SCALE;
	/* masil: leave large watch BSS untouched until the first watch. */
	masil_bridge_used_slot_count = 0;
	masil_bridge_watch_pane_refs = 0;
	masil_bridge_watch_clients = 0;
	masil_bridge_lifecycle_clients = 0;
	masil_bridge_view_clients = 0;
	masil_bridge_pending_dirty = 0;
	masil_bridge_journal_head = 0;
	masil_bridge_journal_count = 0;
	masil_bridge_event_seq = 0;
	masil_bridge_event_exhausted = 0;
	masil_bridge_stream_epoch = 0;
	masil_bridge_stream_exhausted = 0;
	masil_bridge_exiting = 0;
	masil_bridge_dirty_timer_active = 0;
	masil_bridge_dirty_timer_due = 0;
	evtimer_set(&masil_bridge_dirty_event, masil_bridge_dirty_callback, NULL);
	masil_bridge_enabled = 1;
	event_set(&masil_bridge_event, masil_bridge_fd, EV_READ|EV_PERSIST,
	    masil_bridge_accept, NULL);
	event_add(&masil_bridge_event, NULL);
	log_debug("masil bridge: listening on %s", masil_bridge_path);
	return;

fail_bound:
	saved_errno = errno;
	if (lstat(masil_bridge_path, &sb) == 0 &&
	    sb.st_dev == masil_bridge_socket_dev &&
	    sb.st_ino == masil_bridge_socket_ino)
		unlink(masil_bridge_path);
	errno = saved_errno;
fail:
	log_debug("masil bridge: failed to listen on %s: %s",
	    masil_bridge_path, strerror(errno));
	if (masil_bridge_fd != -1) {
		close(masil_bridge_fd);
		masil_bridge_fd = -1;
	}
	masil_bridge_path_derived = 0;
	masil_bridge_path[0] = '\0';
}

/* masil: SIGUSR1 restores an unlinked endpoint without dropping live peers. */
void
masil_bridge_rebind(void)
{
	struct sockaddr_un	 address;
	struct stat		 bound, current;
	char			 derived_path[sizeof masil_bridge_path], cause[256];
	mode_t			 old_mask;
	int			 fd = -1, saved_errno;

	if (!masil_bridge_enabled || masil_bridge_fd == -1 ||
	    masil_bridge_path[0] == '\0')
		return;
	if (lstat(masil_bridge_path, &current) == 0) {
		if (current.st_dev == masil_bridge_socket_dev &&
		    current.st_ino == masil_bridge_socket_ino)
			return;
		log_debug("masil bridge: refusing changed socket path %s",
		    masil_bridge_path);
		return;
	}
	if (errno != ENOENT) {
		log_debug("masil bridge: refusing unsafe rebind path %s",
		    masil_bridge_path);
		return;
	}
	if (masil_bridge_path_derived) {
		if (masil_bridge_default_path(derived_path, sizeof derived_path, cause,
		    sizeof cause) != 0 || strcmp(derived_path, masil_bridge_path) != 0) {
			log_debug("masil bridge: refusing unsafe default rebind path %s",
			    masil_bridge_path);
			return;
		}
	} else if (masil_bridge_private_parent(masil_bridge_path) != 0) {
		log_debug("masil bridge: refusing unsafe rebind path %s",
		    masil_bridge_path);
		return;
	}
	fd = socket(AF_UNIX, SOCK_STREAM, 0);
	if (fd == -1)
		goto fail;
	if (fcntl(fd, F_SETFD, FD_CLOEXEC) == -1)
		goto fail;
	memset(&address, 0, sizeof address);
	address.sun_family = AF_UNIX;
	if (strlcpy(address.sun_path, masil_bridge_path,
	    sizeof address.sun_path) >= sizeof address.sun_path) {
		errno = ENAMETOOLONG;
		goto fail;
	}
	old_mask = umask(S_IRWXG|S_IRWXO);
	if (bind(fd, (struct sockaddr *)&address, sizeof address) != 0) {
		saved_errno = errno;
		umask(old_mask);
		errno = saved_errno;
		goto fail;
	}
	(void)umask(old_mask);
	if (lstat(masil_bridge_path, &bound) != 0)
		goto fail;
	if (!S_ISSOCK(bound.st_mode) || bound.st_uid != geteuid() ||
	    chmod(masil_bridge_path, S_IRUSR|S_IWUSR) != 0 ||
	    listen(fd, MASIL_BRIDGE_CLIENTS) != 0)
		goto fail_bound;
	setblocking(fd, 0);
	event_del(&masil_bridge_event);
	close(masil_bridge_fd);
	masil_bridge_fd = fd;
	masil_bridge_socket_dev = bound.st_dev;
	masil_bridge_socket_ino = bound.st_ino;
	event_set(&masil_bridge_event, masil_bridge_fd, EV_READ|EV_PERSIST,
	    masil_bridge_accept, NULL);
	event_add(&masil_bridge_event, NULL);
	log_debug("masil bridge: rebound %s", masil_bridge_path);
	return;

fail_bound:
	saved_errno = errno;
	if (lstat(masil_bridge_path, &current) == 0 &&
	    current.st_dev == bound.st_dev && current.st_ino == bound.st_ino)
		unlink(masil_bridge_path);
	errno = saved_errno;
fail:
	log_debug("masil bridge: failed to rebind %s: %s", masil_bridge_path,
	    strerror(errno));
	if (fd != -1)
		close(fd);
}

void
masil_bridge_stop(void)
{
	struct masil_bridge_client *client;
	struct stat		  sb;

	if (!masil_bridge_enabled)
		return;
	event_del(&masil_bridge_event);
	close(masil_bridge_fd);
	masil_bridge_fd = -1;
	for (client = masil_bridge_clients; client != NULL; client = client->next)
		masil_bridge_client_stop_flush(client);
	while ((client = masil_bridge_clients) != NULL)
		masil_bridge_client_close(client);
	if (masil_bridge_dirty_timer_active) {
		event_del(&masil_bridge_dirty_event);
		masil_bridge_dirty_timer_active = 0;
	}
	if (lstat(masil_bridge_path, &sb) == 0 &&
	    sb.st_dev == masil_bridge_socket_dev &&
	    sb.st_ino == masil_bridge_socket_ino)
		unlink(masil_bridge_path);
	masil_bridge_enabled = 0;
	masil_bridge_path_derived = 0;
	masil_bridge_path[0] = '\0';
}

/*
 * The server is about to tear down every session. `server_exiting` is a
 * terminal protocol event with no object ID: every watch receives it, and
 * later pane/window/session removals must not look like ordinary closures.
 */
void
masil_bridge_server_exiting(void)
{
	struct masil_bridge_journal_event *record;

	if (!masil_bridge_enabled || masil_bridge_exiting)
		return;
	masil_bridge_exiting = 1;
	record = masil_bridge_journal_new(MASIL_BRIDGE_SERVER_EXITING);
	if (record != NULL)
		masil_bridge_journal_finish();
}

void
masil_bridge_pane_created(struct window_pane *wp)
{
	wp->masil_watch_slot = 0;
	wp->masil_pty_generation = 0;
	wp->masil_screen_generation = masil_bridge_enabled ? 1 : 0;
	wp->masil_generation_exhausted = 0;
	if (!masil_bridge_enabled)
		return;
	if (masil_bridge_bump(&masil_bridge_revision) != 0)
		masil_bridge_revision_exhausted = wp->masil_generation_exhausted = 1;
	/* masil: creation is global only while lifecycle observation is active. */
	if (!masil_bridge_revision_exhausted &&
	    masil_bridge_lifecycle_clients != 0)
		masil_bridge_journal_append_pane(NULL, wp, MASIL_BRIDGE_CREATED);
	if (masil_bridge_revision_exhausted)
		masil_bridge_watch_fail_closed();
}

void
masil_bridge_pane_destroyed(struct window_pane *wp)
{
	struct masil_bridge_watch_slot *slot;

	if (!masil_bridge_enabled)
		return;
	if ((slot = masil_bridge_watch_slot(wp)) != NULL) {
		masil_bridge_dirty_cancel(slot);
		if (!masil_bridge_exiting)
			masil_bridge_journal_append_pane(slot, wp,
			    MASIL_BRIDGE_REMOVED);
		slot->wp = NULL;
		wp->masil_watch_slot = 0;
	} else if (!masil_bridge_exiting && masil_bridge_lifecycle_clients != 0)
		masil_bridge_journal_append_pane(NULL, wp, MASIL_BRIDGE_REMOVED);
	if (masil_bridge_bump(&masil_bridge_revision) != 0)
		masil_bridge_revision_exhausted = wp->masil_generation_exhausted = 1;
	if (masil_bridge_revision_exhausted)
		masil_bridge_watch_fail_closed();
}

void
masil_bridge_pane_state_changed(struct window_pane *wp)
{
	struct masil_bridge_watch_slot *slot;

	if (!masil_bridge_enabled)
		return;
	if ((slot = masil_bridge_watch_slot(wp)) != NULL) {
		masil_bridge_dirty_cancel(slot);
		masil_bridge_journal_append_pane(slot, wp,
		    MASIL_BRIDGE_EXITED);
	} else if (masil_bridge_lifecycle_clients != 0)
		masil_bridge_journal_append_pane(NULL, wp, MASIL_BRIDGE_EXITED);
	if (masil_bridge_bump(&masil_bridge_revision) != 0)
		masil_bridge_revision_exhausted = wp->masil_generation_exhausted = 1;
	if (masil_bridge_revision_exhausted)
		masil_bridge_watch_fail_closed();
}

void
masil_bridge_pty_changed(struct window_pane *wp)
{
	struct masil_bridge_watch_slot *slot;
	int exhausted = 0;

	if (masil_bridge_bump(&wp->masil_pty_generation) != 0)
		exhausted = wp->masil_generation_exhausted = 1;
	if (!masil_bridge_enabled)
		return;
	if (!exhausted && masil_bridge_bump(&wp->masil_screen_generation) != 0)
		exhausted = wp->masil_generation_exhausted = 1;
	if (exhausted)
		masil_bridge_watch_fail_closed();
	else if ((slot = masil_bridge_watch_slot(wp)) != NULL) {
		masil_bridge_dirty_cancel(slot);
		masil_bridge_journal_append_pane(slot, wp,
		    MASIL_BRIDGE_PTY_CHANGED);
	} else if (masil_bridge_lifecycle_clients != 0)
		masil_bridge_journal_append_pane(NULL, wp,
		    MASIL_BRIDGE_PTY_CHANGED);
	if (masil_bridge_bump(&masil_bridge_revision) != 0)
		masil_bridge_revision_exhausted = wp->masil_generation_exhausted = 1;
	if (masil_bridge_revision_exhausted)
		masil_bridge_watch_fail_closed();
}

void
masil_bridge_output_changed(struct window_pane *wp)
{
	struct masil_bridge_watch_slot *slot;
	uint64_t now;

	if (!masil_bridge_enabled)
		return;
	if (masil_bridge_bump(&wp->masil_screen_generation) != 0) {
		wp->masil_generation_exhausted = 1;
		masil_bridge_watch_fail_closed();
		return;
	}
	if ((slot = masil_bridge_watch_slot(wp)) == NULL)
		return;
	slot->screen_generation = wp->masil_screen_generation;
	if (slot->dirty) {
		masil_bridge_stats.coalesced++;
		return;
	}
	now = masil_bridge_now_usec();
	slot->dirty = 1;
	if (slot->dirty_after < now)
		slot->dirty_after = now;
	masil_bridge_pending_dirty++;
	masil_bridge_dirty_schedule(slot->dirty_after);
}

void
masil_bridge_geometry_changed(struct window_pane *wp)
{
	struct masil_bridge_watch_slot *slot;
	int exhausted = 0;

	if (!masil_bridge_enabled)
		return;
	if (masil_bridge_bump(&wp->masil_screen_generation) != 0)
		exhausted = wp->masil_generation_exhausted = 1;
	if (exhausted)
		masil_bridge_watch_fail_closed();
	else if ((slot = masil_bridge_watch_slot(wp)) != NULL) {
		masil_bridge_dirty_cancel(slot);
		masil_bridge_journal_append_pane(slot, wp,
		    MASIL_BRIDGE_RESIZED);
	}
	if (masil_bridge_bump(&masil_bridge_revision) != 0)
		masil_bridge_revision_exhausted = wp->masil_generation_exhausted = 1;
	if (masil_bridge_revision_exhausted)
		masil_bridge_watch_fail_closed();
}
