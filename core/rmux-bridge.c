/*
 * Copyright (c) 2026 rmux contributors
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
#include <stdarg.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>
#include <unistd.h>

#include "tmux.h"
#include "rmux-bridge.h"
#include "yyjson.h"

#define RMUX_BRIDGE_RX_MAX		(8 * 1024)
#define RMUX_BRIDGE_TX_MAX		(64 * 1024)
#define RMUX_BRIDGE_CODEC_POOL		(512 * 1024)
#define RMUX_BRIDGE_TX_TOTAL		(512 * 1024)
#define RMUX_BRIDGE_CLIENTS		32
#define RMUX_BRIDGE_UNAUTHENTICATED	4
#define RMUX_BRIDGE_PAGE		64
#define RMUX_BRIDGE_ROWS		32
#define RMUX_BRIDGE_COLUMNS		240
#define RMUX_BRIDGE_TEXT		(16 * 1024)
#define RMUX_BRIDGE_TEXT_ENCODED		(40 * 1024)
#define RMUX_BRIDGE_BATCH_FRAMES		4
#define RMUX_BRIDGE_BATCH_BYTES		(32 * 1024)
#define RMUX_BRIDGE_BATCH_USEC		200
#define RMUX_BRIDGE_CLIENT_TIMEOUT	5
#define RMUX_BRIDGE_SNAPSHOT_RATE	64
#define RMUX_BRIDGE_SNAPSHOT_BYTES	(512 * 1024)
#define RMUX_BRIDGE_SNAPSHOT_BURST	(128 * 1024)
#define RMUX_BRIDGE_CREDIT_SCALE		1000000ULL

struct rmux_bridge_client {
	int			 fd;
	int			 hello_done;
	int			 close_after_write;
	struct event		 event;
	struct event		 timeout_event;
	u_char			 rx[4 + RMUX_BRIDGE_RX_MAX];
	size_t			 rx_len;
	u_char			*tx;
	size_t			 tx_len;
	size_t			 tx_off;
	struct rmux_bridge_client *next;
};

struct rmux_bridge_stats {
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
};

struct rmux_json_builder {
	char	*buf;
	size_t	 len;
	size_t	 cap;
	int	 failed;
};

static int			 rmux_bridge_enabled;
static int			 rmux_bridge_fd = -1;
static struct event		 rmux_bridge_event;
static char			 rmux_bridge_path[sizeof ((struct sockaddr_un *)0)->sun_path];
static dev_t			 rmux_bridge_socket_dev;
static ino_t			 rmux_bridge_socket_ino;
static char			 rmux_bridge_boot_id[37];
static uint64_t			 rmux_bridge_revision = 1;
static int			 rmux_bridge_revision_exhausted;
static struct rmux_bridge_client *rmux_bridge_clients;
static size_t			 rmux_bridge_client_count;
static size_t			 rmux_bridge_unauthenticated;
static size_t			 rmux_bridge_tx_bytes;
static uint64_t			 rmux_bridge_snapshot_updated;
static uint64_t			 rmux_bridge_snapshot_credit;
static uint64_t			 rmux_bridge_snapshot_byte_credit;
static struct rmux_bridge_stats	 rmux_bridge_stats;
static u_char			 rmux_bridge_codec_pool[RMUX_BRIDGE_CODEC_POOL];
static char			 rmux_bridge_response[RMUX_BRIDGE_TX_MAX];

static void printflike(2, 3) rmux_json_printf(struct rmux_json_builder *,
    const char *, ...);

static uint32_t
rmux_bridge_get_u32(const u_char *p)
{
	return (((uint32_t)p[0] << 24) | ((uint32_t)p[1] << 16) |
	    ((uint32_t)p[2] << 8) | (uint32_t)p[3]);
}

static void
rmux_bridge_put_u32(u_char *p, uint32_t value)
{
	p[0] = (value >> 24) & 0xff;
	p[1] = (value >> 16) & 0xff;
	p[2] = (value >> 8) & 0xff;
	p[3] = value & 0xff;
}

static uint64_t
rmux_bridge_now_usec(void)
{
	struct timespec ts;

	if (clock_gettime(CLOCK_MONOTONIC, &ts) != 0)
		clock_gettime(CLOCK_REALTIME, &ts);
	return ((uint64_t)ts.tv_sec * 1000000ULL + ts.tv_nsec / 1000);
}

static void
rmux_bridge_new_boot_id(void)
{
	u_char data[16];

	arc4random_buf(data, sizeof data);
	data[6] = (data[6] & 0x0f) | 0x40;
	data[8] = (data[8] & 0x3f) | 0x80;
	snprintf(rmux_bridge_boot_id, sizeof rmux_bridge_boot_id,
	    "%02x%02x%02x%02x-%02x%02x-%02x%02x-%02x%02x-"
	    "%02x%02x%02x%02x%02x%02x",
	    data[0], data[1], data[2], data[3], data[4], data[5], data[6],
	    data[7], data[8], data[9], data[10], data[11], data[12], data[13],
	    data[14], data[15]);
}

static int
rmux_bridge_bump(uint64_t *value)
{
	if (*value == UINT64_MAX)
		return (-1);
	(*value)++;
	return (0);
}

static void
rmux_json_putn(struct rmux_json_builder *builder, const char *value,
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
rmux_json_puts(struct rmux_json_builder *builder, const char *value)
{
	rmux_json_putn(builder, value, strlen(value));
}

static void
rmux_json_printf(struct rmux_json_builder *builder, const char *fmt, ...)
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
rmux_json_quote(struct rmux_json_builder *builder, const char *value,
    size_t length)
{
	static const char hex[] = "0123456789abcdef";
	size_t		 i;
	u_char		 ch;
	char		 escaped[6];

	rmux_json_puts(builder, "\"");
	for (i = 0; i < length; i++) {
		ch = value[i];
		switch (ch) {
		case '\"':
			rmux_json_puts(builder, "\\\"");
			break;
		case '\\':
			rmux_json_puts(builder, "\\\\");
			break;
		case '\b':
			rmux_json_puts(builder, "\\b");
			break;
		case '\f':
			rmux_json_puts(builder, "\\f");
			break;
		case '\n':
			rmux_json_puts(builder, "\\n");
			break;
		case '\r':
			rmux_json_puts(builder, "\\r");
			break;
		case '\t':
			rmux_json_puts(builder, "\\t");
			break;
		default:
			if (ch < 0x20) {
				escaped[0] = '\\';
				escaped[1] = 'u';
				escaped[2] = '0';
				escaped[3] = '0';
				escaped[4] = hex[ch >> 4];
				escaped[5] = hex[ch & 0xf];
				rmux_json_putn(builder, escaped, sizeof escaped);
			} else
				rmux_json_putn(builder, (const char *)&value[i], 1);
			break;
		}
	}
	rmux_json_puts(builder, "\"");
}

static size_t
rmux_json_quoted_length(const char *value, size_t length)
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
rmux_bridge_client_update(struct rmux_bridge_client *client);

static void
rmux_bridge_client_close(struct rmux_bridge_client *client)
{
	struct rmux_bridge_client **previous, *found;

	event_del(&client->event);
	if (event_initialized(&client->timeout_event))
		event_del(&client->timeout_event);
	close(client->fd);
	if (client->tx != NULL) {
		rmux_bridge_tx_bytes -= client->tx_len;
		free(client->tx);
	}
	if (!client->hello_done && rmux_bridge_unauthenticated != 0)
		rmux_bridge_unauthenticated--;
	previous = &rmux_bridge_clients;
	while ((found = *previous) != NULL) {
		if (found == client) {
			*previous = client->next;
			break;
		}
		previous = &found->next;
	}
	if (rmux_bridge_client_count != 0)
		rmux_bridge_client_count--;
	rmux_bridge_stats.disconnects++;
	free(client);
}

static int
rmux_bridge_queue(struct rmux_bridge_client *client, const char *json,
    size_t length)
{
	size_t frame_length = length + 4;

	if (length == 0 || length > RMUX_BRIDGE_TX_MAX || client->tx != NULL ||
	    frame_length > RMUX_BRIDGE_TX_TOTAL - rmux_bridge_tx_bytes)
		return (-1);
	client->tx = xmalloc(frame_length);
	rmux_bridge_put_u32(client->tx, length);
	memcpy(client->tx + 4, json, length);
	client->tx_len = frame_length;
	client->tx_off = 0;
	rmux_bridge_tx_bytes += frame_length;
	rmux_bridge_stats.tx_frames++;
	rmux_bridge_client_update(client);
	return (0);
}

static void
rmux_bridge_client_timeout(__unused int fd, __unused short events, void *data)
{
	struct rmux_bridge_client *client = data;

	rmux_bridge_stats.protocol_errors++;
	rmux_bridge_client_close(client);
}

static void
rmux_bridge_client_deadline(struct rmux_bridge_client *client)
{
	struct timeval timeout = { .tv_sec = RMUX_BRIDGE_CLIENT_TIMEOUT };
	int active;

	active = !client->hello_done || client->rx_len != 0 ||
	    client->tx != NULL;
	if (event_initialized(&client->timeout_event))
		event_del(&client->timeout_event);
	if (!active)
		return;
	evtimer_set(&client->timeout_event, rmux_bridge_client_timeout, client);
	evtimer_add(&client->timeout_event, &timeout);
}

static int
rmux_bridge_queue_builder(struct rmux_bridge_client *client,
    struct rmux_json_builder *builder)
{
	if (builder->failed || builder->len == 0)
		return (-1);
	return (rmux_bridge_queue(client, builder->buf, builder->len));
}

static int
rmux_bridge_error(struct rmux_bridge_client *client, const char *request_id,
    size_t request_id_length, const char *code, const char *message)
{
	struct rmux_json_builder builder = {
	    rmux_bridge_response, 0, sizeof rmux_bridge_response, 0
	};

	rmux_json_puts(&builder, "{\"v\":1,\"kind\":\"error\"");
	if (request_id != NULL) {
		rmux_json_puts(&builder, ",\"request_id\":");
		rmux_json_quote(&builder, request_id, request_id_length);
	}
	rmux_json_puts(&builder, ",\"code\":");
	rmux_json_quote(&builder, code, strlen(code));
	rmux_json_puts(&builder, ",\"message\":");
	rmux_json_quote(&builder, message, strlen(message));
	rmux_json_puts(&builder, "}");
	rmux_bridge_stats.rejected++;
	return (rmux_bridge_queue_builder(client, &builder));
}

static int
rmux_bridge_key_allowed(const char *key, size_t length,
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
rmux_bridge_schema(yyjson_val *root, const char *const *allowed,
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
		if (!rmux_bridge_key_allowed(string, length, allowed,
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
rmux_bridge_parse_decimal(yyjson_val *value, uint64_t *result)
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
rmux_bridge_hello(struct rmux_bridge_client *client, const char *request_id,
    size_t request_id_length)
{
	struct rmux_json_builder builder = {
	    rmux_bridge_response, 0, sizeof rmux_bridge_response, 0
	};

	rmux_json_puts(&builder,
	    "{\"v\":1,\"kind\":\"hello\",\"request_id\":");
	rmux_json_quote(&builder, request_id, request_id_length);
	rmux_json_puts(&builder, ",\"core_boot_id\":");
	rmux_json_quote(&builder, rmux_bridge_boot_id,
	    strlen(rmux_bridge_boot_id));
	rmux_json_puts(&builder,
	    ",\"negotiated_version\":{\"major\":1,\"minor\":0},"
	    "\"capabilities\":{\"inventory\":true,\"snapshot\":true,"
	    "\"stats\":true,\"watch\":false,\"events\":false,"
	    "\"actions\":false,\"submit\":false},"
	    "\"limits\":{\"rx_frame\":8192,\"tx_frame\":65536,"
	    "\"json_depth\":8,\"inventory_page\":64,"
	    "\"snapshot_rows\":32,\"snapshot_columns\":240,"
	    "\"snapshot_text\":16384}}");
	if (rmux_bridge_queue_builder(client, &builder) != 0)
		return (-1);
	client->hello_done = 1;
	if (rmux_bridge_unauthenticated != 0)
		rmux_bridge_unauthenticated--;
	return (0);
}

static int
rmux_bridge_inventory(struct rmux_bridge_client *client,
    const char *request_id, size_t request_id_length, yyjson_val *root)
{
	struct rmux_json_builder builder = {
	    rmux_bridge_response, 0, sizeof rmux_bridge_response, 0
	};
	struct window_pane	*wp, lookup, *page[RMUX_BRIDGE_PAGE + 1];
	yyjson_val		*value;
	uint64_t		 requested_revision, cursor = 0;
	size_t		 count = 0, i, shown;
	int		 has_cursor = 0, has_revision = 0;

	value = yyjson_obj_get(root, "cursor");
	if (rmux_bridge_revision_exhausted)
		return (rmux_bridge_error(client, request_id, request_id_length,
		    "generation_exhausted", "inventory revision cannot be reused"));
	if (value != NULL) {
		if (rmux_bridge_parse_decimal(value, &cursor) != 0 ||
		    cursor > UINT_MAX)
			return (rmux_bridge_error(client, request_id,
			    request_id_length, "invalid_cursor",
			    "cursor must be a decimal pane id"));
		has_cursor = 1;
	}
	value = yyjson_obj_get(root, "revision");
	if (value != NULL) {
		if (rmux_bridge_parse_decimal(value, &requested_revision) != 0)
			return (rmux_bridge_error(client, request_id,
			    request_id_length, "invalid_revision",
			    "revision must be a decimal string"));
		has_revision = 1;
	}
	if (has_cursor && !has_revision)
		return (rmux_bridge_error(client, request_id, request_id_length,
		    "revision_required", "continuation requires revision"));
	if (has_revision && requested_revision != rmux_bridge_revision)
		return (rmux_bridge_error(client, request_id, request_id_length,
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
	shown = count > RMUX_BRIDGE_PAGE ? RMUX_BRIDGE_PAGE : count;
	rmux_json_puts(&builder,
	    "{\"v\":1,\"kind\":\"inventory\",\"request_id\":");
	rmux_json_quote(&builder, request_id, request_id_length);
	rmux_json_printf(&builder, ",\"core_boot_id\":\"%s\","
	    "\"revision\":\"%llu\",\"panes\":[",
	    rmux_bridge_boot_id, (unsigned long long)rmux_bridge_revision);
	for (i = 0; i < shown; i++) {
		wp = page[i];
		if (wp->rmux_generation_exhausted)
			return (rmux_bridge_error(client, request_id,
			    request_id_length, "generation_exhausted",
			    "pane generations cannot be reused"));
		if (i != 0)
			rmux_json_puts(&builder, ",");
		rmux_json_printf(&builder,
		    "{\"pane_id\":\"%%%u\",\"pty_generation\":\"%llu\","
		    "\"screen_generation\":\"%llu\",\"width\":%u,"
		    "\"height\":%u,\"dead\":%s}", wp->id,
		    (unsigned long long)wp->rmux_pty_generation,
		    (unsigned long long)wp->rmux_screen_generation,
		    screen_size_x(&wp->base), screen_size_y(&wp->base),
		    (wp->flags & (PANE_EXITED|PANE_DESTROYED)) ? "true" : "false");
	}
	rmux_json_puts(&builder, "],\"next_cursor\":");
	if (count > RMUX_BRIDGE_PAGE)
		rmux_json_printf(&builder, "\"%u\"", page[shown - 1]->id);
	else
		rmux_json_puts(&builder, "null");
	rmux_json_printf(&builder, ",\"complete\":%s}",
	    count > RMUX_BRIDGE_PAGE ? "false" : "true");
	rmux_bridge_stats.inventory_requests++;
	return (rmux_bridge_queue_builder(client, &builder));
}

static int
rmux_bridge_pane_id(yyjson_val *value, u_int *pane_id)
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

static int
rmux_bridge_snapshot_admit(size_t estimated_bytes)
{
	uint64_t now, elapsed, add;
	uint64_t request_max = RMUX_BRIDGE_SNAPSHOT_RATE *
	    RMUX_BRIDGE_CREDIT_SCALE;
	uint64_t byte_max = RMUX_BRIDGE_SNAPSHOT_BURST *
	    RMUX_BRIDGE_CREDIT_SCALE;
	uint64_t request_cost = RMUX_BRIDGE_CREDIT_SCALE;
	uint64_t byte_cost = estimated_bytes * RMUX_BRIDGE_CREDIT_SCALE;

	now = rmux_bridge_now_usec();
	elapsed = now - rmux_bridge_snapshot_updated;
	rmux_bridge_snapshot_updated = now;
	if (elapsed >= 2000000) {
		rmux_bridge_snapshot_credit = request_max;
		rmux_bridge_snapshot_byte_credit = byte_max;
	} else {
		add = elapsed * RMUX_BRIDGE_SNAPSHOT_RATE;
		if (add >= request_max - rmux_bridge_snapshot_credit)
			rmux_bridge_snapshot_credit = request_max;
		else
			rmux_bridge_snapshot_credit += add;
		add = elapsed * RMUX_BRIDGE_SNAPSHOT_BYTES;
		if (add >= byte_max - rmux_bridge_snapshot_byte_credit)
			rmux_bridge_snapshot_byte_credit = byte_max;
		else
			rmux_bridge_snapshot_byte_credit += add;
	}
	if (rmux_bridge_snapshot_credit < request_cost ||
	    rmux_bridge_snapshot_byte_credit < byte_cost) {
		rmux_bridge_stats.rate_limited++;
		return (-1);
	}
	rmux_bridge_snapshot_credit -= request_cost;
	rmux_bridge_snapshot_byte_credit -= byte_cost;
	return (0);
}

static int
rmux_bridge_snapshot_adjust(size_t estimated_bytes, size_t actual_bytes)
{
	uint64_t byte_max = RMUX_BRIDGE_SNAPSHOT_BURST *
	    RMUX_BRIDGE_CREDIT_SCALE;
	uint64_t difference;

	if (actual_bytes <= estimated_bytes) {
		difference = (estimated_bytes - actual_bytes) *
		    RMUX_BRIDGE_CREDIT_SCALE;
		if (difference >= byte_max - rmux_bridge_snapshot_byte_credit)
			rmux_bridge_snapshot_byte_credit = byte_max;
		else
			rmux_bridge_snapshot_byte_credit += difference;
		return (0);
	}
	difference = (actual_bytes - estimated_bytes) *
	    RMUX_BRIDGE_CREDIT_SCALE;
	if (rmux_bridge_snapshot_byte_credit < difference) {
		rmux_bridge_stats.rate_limited++;
		return (-1);
	}
	rmux_bridge_snapshot_byte_credit -= difference;
	return (0);
}

static int
rmux_bridge_snapshot(struct rmux_bridge_client *client,
	const char *request_id, size_t request_id_length, yyjson_val *root)
{
	struct rmux_json_builder builder = {
	    rmux_bridge_response, 0, sizeof rmux_bridge_response, 0
	};
	struct window_pane	*wp;
	struct screen		*screen;
	struct grid_cell	 cell;
	yyjson_val		*value;
	char			 text[RMUX_BRIDGE_TEXT];
	uint64_t		 generation, pty_generation, expected;
	size_t		 text_length = 0, encoded_length = 0;
	size_t		 clipped_bytes = 0, cell_encoded;
	u_int		 pane_id, width, height, columns, rows, first, x, y;
	size_t		 estimated_bytes;
	int		 accepting = 1;

	value = yyjson_obj_get(root, "pane_id");
	if (rmux_bridge_pane_id(value, &pane_id) != 0)
		return (rmux_bridge_error(client, request_id, request_id_length,
		    "invalid_pane_id", "pane_id must have the form %N"));
	wp = window_pane_find_by_id(pane_id);
	if (wp == NULL)
		return (rmux_bridge_error(client, request_id, request_id_length,
		    "target_gone", "pane does not exist"));
	if (wp->rmux_generation_exhausted)
		return (rmux_bridge_error(client, request_id, request_id_length,
		    "generation_exhausted", "pane generations cannot be reused"));
	value = yyjson_obj_get(root, "expected_core_boot_id");
	if (value != NULL && (!yyjson_is_str(value) ||
	    yyjson_get_len(value) != strlen(rmux_bridge_boot_id) ||
	    memcmp(yyjson_get_str(value), rmux_bridge_boot_id,
	    yyjson_get_len(value)) != 0))
		return (rmux_bridge_error(client, request_id, request_id_length,
		    "boot_mismatch", "core boot id does not match"));
	value = yyjson_obj_get(root, "expected_pty_generation");
	if (value != NULL && (rmux_bridge_parse_decimal(value, &expected) != 0 ||
	    expected != wp->rmux_pty_generation))
		return (rmux_bridge_error(client, request_id, request_id_length,
		    "pty_generation_mismatch", "pane PTY generation does not match"));
	value = yyjson_obj_get(root, "expected_screen_generation");
	if (value != NULL && (rmux_bridge_parse_decimal(value, &expected) != 0 ||
	    expected != wp->rmux_screen_generation))
		return (rmux_bridge_error(client, request_id, request_id_length,
		    "screen_generation_mismatch",
		    "pane screen generation does not match"));
	pty_generation = wp->rmux_pty_generation;
	generation = wp->rmux_screen_generation;
	screen = &wp->base;
	width = screen_size_x(screen);
	height = screen_size_y(screen);
	columns = width > RMUX_BRIDGE_COLUMNS ? RMUX_BRIDGE_COLUMNS : width;
	rows = height > RMUX_BRIDGE_ROWS ? RMUX_BRIDGE_ROWS : height;
	first = screen_hsize(screen) + height - rows;
	estimated_bytes = (size_t)rows * columns * 4;
	if (estimated_bytes > RMUX_BRIDGE_TEXT)
		estimated_bytes = RMUX_BRIDGE_TEXT;
	if (rmux_bridge_snapshot_admit(estimated_bytes) != 0)
		return (rmux_bridge_error(client, request_id, request_id_length,
		    "rate_limited", "snapshot budget is temporarily exhausted"));

	for (y = 0; y < rows; y++) {
		for (x = 0; x < columns; x++) {
			grid_get_cell(screen->grid, x, first + y, &cell);
			if (cell.flags & GRID_FLAG_PADDING)
				continue;
			cell_encoded = rmux_json_quoted_length(
			    (const char *)cell.data.data, cell.data.size);
			if (accepting &&
			    text_length + cell.data.size <= sizeof text &&
			    encoded_length + cell_encoded <=
			    RMUX_BRIDGE_TEXT_ENCODED) {
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
			    encoded_length + 2 <= RMUX_BRIDGE_TEXT_ENCODED) {
				text[text_length++] = '\n';
				encoded_length += 2;
			} else {
				accepting = 0;
				clipped_bytes++;
			}
		}
	}
	if (rmux_bridge_snapshot_adjust(estimated_bytes, text_length) != 0)
		return (rmux_bridge_error(client, request_id, request_id_length,
		    "rate_limited", "snapshot byte budget is temporarily exhausted"));

	rmux_json_puts(&builder,
	    "{\"v\":1,\"kind\":\"snapshot\",\"request_id\":");
	rmux_json_quote(&builder, request_id, request_id_length);
	rmux_json_printf(&builder,
	    ",\"core_boot_id\":\"%s\",\"pane_id\":\"%%%u\","
	    "\"pty_generation\":\"%llu\",\"screen_generation\":\"%llu\","
	    "\"width\":%u,\"height\":%u,\"cursor\":{\"x\":%u,\"y\":%u},"
	    "\"screen_kind\":\"%s\",\"source\":{\"start_row\":%u,"
	    "\"rows\":%u,\"columns\":%u},\"clipped\":{\"rows\":%u,"
	    "\"columns\":%u,\"bytes\":%zu},\"complete\":%s,\"text\":",
	    rmux_bridge_boot_id, wp->id,
	    (unsigned long long)pty_generation,
	    (unsigned long long)generation, width, height, screen->cx, screen->cy,
	    SCREEN_IS_ALTERNATE(screen) ? "alternate" : "main", height - rows,
	    rows, columns, height - rows, width - columns, clipped_bytes,
	    generation == wp->rmux_screen_generation &&
	    pty_generation == wp->rmux_pty_generation && clipped_bytes == 0 &&
	    rows == height && columns == width ?
	    "true" : "false");
	rmux_json_quote(&builder, text, text_length);
	rmux_json_puts(&builder, "}");
	rmux_bridge_stats.snapshots++;
	return (rmux_bridge_queue_builder(client, &builder));
}

static int
rmux_bridge_stats_response(struct rmux_bridge_client *client,
	const char *request_id, size_t request_id_length)
{
	struct rmux_json_builder builder = {
	    rmux_bridge_response, 0, sizeof rmux_bridge_response, 0
	};

	rmux_json_puts(&builder,
	    "{\"v\":1,\"kind\":\"stats\",\"request_id\":");
	rmux_json_quote(&builder, request_id, request_id_length);
	rmux_json_printf(&builder,
	    ",\"core_boot_id\":\"%s\",\"revision\":\"%llu\","
	    "\"revision_exhausted\":%s,\"clients\":%zu,"
	    "\"tx_queued_bytes\":%zu,\"counters\":{"
	    "\"connects\":\"%llu\",\"disconnects\":\"%llu\","
	    "\"rx_frames\":\"%llu\",\"tx_frames\":\"%llu\","
	    "\"rx_bytes\":\"%llu\",\"tx_bytes\":\"%llu\","
	    "\"requests\":\"%llu\",\"rejected\":\"%llu\","
	    "\"protocol_errors\":\"%llu\","
	    "\"inventory_requests\":\"%llu\",\"snapshots\":\"%llu\","
	    "\"rate_limited\":\"%llu\"}}",
	    rmux_bridge_boot_id, (unsigned long long)rmux_bridge_revision,
	    rmux_bridge_revision_exhausted ? "true" : "false",
	    rmux_bridge_client_count, rmux_bridge_tx_bytes,
	    (unsigned long long)rmux_bridge_stats.connects,
	    (unsigned long long)rmux_bridge_stats.disconnects,
	    (unsigned long long)rmux_bridge_stats.rx_frames,
	    (unsigned long long)rmux_bridge_stats.tx_frames,
	    (unsigned long long)rmux_bridge_stats.rx_bytes,
	    (unsigned long long)rmux_bridge_stats.tx_bytes,
	    (unsigned long long)rmux_bridge_stats.requests,
	    (unsigned long long)rmux_bridge_stats.rejected,
	    (unsigned long long)rmux_bridge_stats.protocol_errors,
	    (unsigned long long)rmux_bridge_stats.inventory_requests,
	    (unsigned long long)rmux_bridge_stats.snapshots,
	    (unsigned long long)rmux_bridge_stats.rate_limited);
	return (rmux_bridge_queue_builder(client, &builder));
}

static int
rmux_bridge_process(struct rmux_bridge_client *client, u_char *payload,
    size_t length)
{
	static const char *const hello_fields[] = {
	    "v", "kind", "request_id"
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
	yyjson_alc	 allocator;
	yyjson_doc	*document;
	yyjson_val	*root, *version, *kind, *request;
	const char	*code = NULL, *request_id = NULL;
	size_t		 request_id_length = 0;
	int		 result;

	if (!yyjson_alc_pool_init(&allocator, rmux_bridge_codec_pool,
	    sizeof rmux_bridge_codec_pool))
		return (-1);
	document = yyjson_read_opts((char *)payload, length, YYJSON_READ_NOFLAG,
	    &allocator, NULL);
	if (document == NULL) {
		rmux_bridge_stats.protocol_errors++;
		return (rmux_bridge_error(client, NULL, 0, "invalid_json",
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
		result = rmux_bridge_error(client, request_id, request_id_length,
		    "invalid_request", "v, kind, and request_id are required");
		goto out;
	}

	if (!client->hello_done && !yyjson_equals_str(kind, "hello")) {
		result = rmux_bridge_error(client, request_id, request_id_length,
		    "handshake_required", "hello must be the first request");
		goto out;
	}
	if (yyjson_equals_str(kind, "hello")) {
		if (client->hello_done) {
			result = rmux_bridge_error(client, request_id,
			    request_id_length, "already_initialized",
			    "hello was already completed");
			goto out;
		}
		if (rmux_bridge_schema(root, hello_fields,
		    nitems(hello_fields), &code) != 0) {
			result = rmux_bridge_error(client, request_id,
			    request_id_length, code, "hello schema rejected");
			goto out;
		}
		result = rmux_bridge_hello(client, request_id, request_id_length);
	} else if (yyjson_equals_str(kind, "inventory")) {
		if (rmux_bridge_schema(root, inventory_fields,
		    nitems(inventory_fields), &code) != 0) {
			result = rmux_bridge_error(client, request_id,
			    request_id_length, code, "inventory schema rejected");
			goto out;
		}
		result = rmux_bridge_inventory(client, request_id,
		    request_id_length, root);
	} else if (yyjson_equals_str(kind, "snapshot")) {
		if (rmux_bridge_schema(root, snapshot_fields,
		    nitems(snapshot_fields), &code) != 0) {
			result = rmux_bridge_error(client, request_id,
			    request_id_length, code, "snapshot schema rejected");
			goto out;
		}
		result = rmux_bridge_snapshot(client, request_id,
		    request_id_length, root);
	} else if (yyjson_equals_str(kind, "stats")) {
		if (rmux_bridge_schema(root, stats_fields,
		    nitems(stats_fields), &code) != 0) {
			result = rmux_bridge_error(client, request_id,
			    request_id_length, code, "stats schema rejected");
			goto out;
		}
		result = rmux_bridge_stats_response(client, request_id,
		    request_id_length);
	} else
		result = rmux_bridge_error(client, request_id, request_id_length,
		    "unsupported_kind", "request kind is not supported");

out:
	rmux_bridge_stats.requests++;
	yyjson_doc_free(document);
	return (result);
}

static int
rmux_bridge_consume(struct rmux_bridge_client *client, u_int *frames,
    size_t *bytes, uint64_t started)
{
	uint32_t length;
	size_t consumed;

	while (client->tx == NULL && client->rx_len >= 4) {
		length = rmux_bridge_get_u32(client->rx);
		if (length == 0 || length > RMUX_BRIDGE_RX_MAX) {
			rmux_bridge_stats.protocol_errors++;
			if (rmux_bridge_error(client, NULL, 0, "invalid_frame_length",
			    "frame length is outside the allowed range") != 0)
				return (-1);
			client->close_after_write = 1;
			return (0);
		}
		if (client->rx_len < (size_t)length + 4)
			return (0);
		consumed = (size_t)length + 4;
		rmux_bridge_stats.rx_frames++;
		if (rmux_bridge_process(client, client->rx + 4, length) != 0)
			return (-1);
		client->rx_len -= consumed;
		if (client->rx_len != 0)
			memmove(client->rx, client->rx + consumed, client->rx_len);
		(*frames)++;
		*bytes += consumed;
		if (*frames >= RMUX_BRIDGE_BATCH_FRAMES ||
		    *bytes >= RMUX_BRIDGE_BATCH_BYTES ||
		    rmux_bridge_now_usec() - started >= RMUX_BRIDGE_BATCH_USEC)
			return (0);
	}
	return (0);
}

static void
rmux_bridge_client_callback(int fd, short events, void *data)
{
	struct rmux_bridge_client *client = data;
	ssize_t			 used;
	size_t			 available, bytes = 0;
	u_int			 frames = 0;
	uint64_t		 started = rmux_bridge_now_usec();
	int			 flags = 0;

	if ((events & EV_WRITE) && client->tx != NULL) {
#ifdef MSG_NOSIGNAL
		flags = MSG_NOSIGNAL;
#endif
		used = send(fd, client->tx + client->tx_off,
		    client->tx_len - client->tx_off, flags);
		if (used == -1 && errno != EAGAIN && errno != EINTR) {
			rmux_bridge_client_close(client);
			return;
		}
		if (used > 0) {
			client->tx_off += used;
			rmux_bridge_stats.tx_bytes += used;
		}
		if (client->tx_off == client->tx_len) {
			rmux_bridge_tx_bytes -= client->tx_len;
			free(client->tx);
			client->tx = NULL;
			client->tx_len = client->tx_off = 0;
			if (client->close_after_write) {
				rmux_bridge_client_close(client);
				return;
			}
		}
	}

	if (client->tx == NULL && rmux_bridge_consume(client, &frames, &bytes,
	    started) != 0) {
		rmux_bridge_client_close(client);
		return;
	}
	if ((events & EV_READ) && client->tx == NULL &&
	    frames < RMUX_BRIDGE_BATCH_FRAMES &&
	    rmux_bridge_now_usec() - started < RMUX_BRIDGE_BATCH_USEC) {
		available = sizeof client->rx - client->rx_len;
		used = recv(fd, client->rx + client->rx_len, available, 0);
		if (used == 0) {
			if (client->rx_len != 0)
				rmux_bridge_stats.protocol_errors++;
			rmux_bridge_client_close(client);
			return;
		}
		if (used == -1 && errno != EAGAIN && errno != EINTR) {
			rmux_bridge_client_close(client);
			return;
		}
		if (used > 0) {
			client->rx_len += used;
			rmux_bridge_stats.rx_bytes += used;
			bytes += used;
			if (rmux_bridge_consume(client, &frames, &bytes,
			    started) != 0) {
				rmux_bridge_client_close(client);
				return;
			}
		}
	}
	rmux_bridge_client_deadline(client);
	rmux_bridge_client_update(client);
}

static void
rmux_bridge_client_update(struct rmux_bridge_client *client)
{
	short events;

	events = client->tx == NULL ? EV_READ : EV_WRITE;
	if (event_initialized(&client->event))
		event_del(&client->event);
	event_set(&client->event, client->fd, events, rmux_bridge_client_callback,
	    client);
	event_add(&client->event, NULL);
}

static void
rmux_bridge_accept(__unused int fd, short events, __unused void *data)
{
	struct sockaddr_un	 address;
	struct rmux_bridge_client *client;
	socklen_t		 length = sizeof address;
	uid_t			 uid;
	gid_t			 gid;
	int			 client_fd;

	if (!(events & EV_READ))
		return;
	client_fd = accept(rmux_bridge_fd, (struct sockaddr *)&address, &length);
	if (client_fd == -1)
		return;
	if (fcntl(client_fd, F_SETFD, FD_CLOEXEC) == -1) {
		close(client_fd);
		return;
	}
	if (rmux_bridge_client_count >= RMUX_BRIDGE_CLIENTS ||
	    rmux_bridge_unauthenticated >= RMUX_BRIDGE_UNAUTHENTICATED ||
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
	client->next = rmux_bridge_clients;
	rmux_bridge_clients = client;
	rmux_bridge_client_count++;
	rmux_bridge_unauthenticated++;
	rmux_bridge_stats.connects++;
	rmux_bridge_client_update(client);
	rmux_bridge_client_deadline(client);
}

static int
rmux_bridge_private_parent(const char *path)
{
	char	 directory[sizeof rmux_bridge_path];
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

void
rmux_bridge_start(void)
{
	const char		*path = getenv("RMUX_BRIDGE_SOCKET");
	struct sockaddr_un	 address;
	struct stat		 sb;
	mode_t			 old_mask;
	int			 saved_errno;

	if (path == NULL || *path == '\0' || rmux_bridge_enabled)
		return;
	if (rmux_bridge_private_parent(path) != 0 ||
	    strlcpy(rmux_bridge_path, path, sizeof rmux_bridge_path) >=
	    sizeof rmux_bridge_path) {
		log_debug("rmux bridge: socket path must be in a private owner directory");
		return;
	}
	if (lstat(rmux_bridge_path, &sb) == 0 || errno != ENOENT) {
		log_debug("rmux bridge: refusing existing socket path %s",
		    rmux_bridge_path);
		return;
	}
	rmux_bridge_fd = socket(AF_UNIX, SOCK_STREAM, 0);
	if (rmux_bridge_fd == -1)
		goto fail;
	if (fcntl(rmux_bridge_fd, F_SETFD, FD_CLOEXEC) == -1)
		goto fail;
	memset(&address, 0, sizeof address);
	address.sun_family = AF_UNIX;
	strlcpy(address.sun_path, rmux_bridge_path, sizeof address.sun_path);
	old_mask = umask(S_IRWXG|S_IRWXO);
	if (bind(rmux_bridge_fd, (struct sockaddr *)&address, sizeof address) != 0) {
		saved_errno = errno;
		umask(old_mask);
		errno = saved_errno;
		goto fail;
	}
	umask(old_mask);
	if (chmod(rmux_bridge_path, S_IRUSR|S_IWUSR) != 0 ||
	    listen(rmux_bridge_fd, RMUX_BRIDGE_CLIENTS) != 0 ||
	    lstat(rmux_bridge_path, &sb) != 0)
		goto fail_bound;
	rmux_bridge_socket_dev = sb.st_dev;
	rmux_bridge_socket_ino = sb.st_ino;
	setblocking(rmux_bridge_fd, 0);
	rmux_bridge_new_boot_id();
	rmux_bridge_snapshot_updated = rmux_bridge_now_usec();
	rmux_bridge_snapshot_credit = RMUX_BRIDGE_SNAPSHOT_RATE *
	    RMUX_BRIDGE_CREDIT_SCALE;
	rmux_bridge_snapshot_byte_credit = RMUX_BRIDGE_SNAPSHOT_BURST *
	    RMUX_BRIDGE_CREDIT_SCALE;
	rmux_bridge_enabled = 1;
	event_set(&rmux_bridge_event, rmux_bridge_fd, EV_READ|EV_PERSIST,
	    rmux_bridge_accept, NULL);
	event_add(&rmux_bridge_event, NULL);
	log_debug("rmux bridge: listening on %s", rmux_bridge_path);
	return;

fail_bound:
	saved_errno = errno;
	if (lstat(rmux_bridge_path, &sb) == 0)
		unlink(rmux_bridge_path);
	errno = saved_errno;
fail:
	log_debug("rmux bridge: failed to listen on %s: %s",
	    rmux_bridge_path, strerror(errno));
	if (rmux_bridge_fd != -1) {
		close(rmux_bridge_fd);
		rmux_bridge_fd = -1;
	}
	rmux_bridge_path[0] = '\0';
}

void
rmux_bridge_stop(void)
{
	struct rmux_bridge_client *client;
	struct stat		  sb;

	if (!rmux_bridge_enabled)
		return;
	event_del(&rmux_bridge_event);
	close(rmux_bridge_fd);
	rmux_bridge_fd = -1;
	while ((client = rmux_bridge_clients) != NULL)
		rmux_bridge_client_close(client);
	if (lstat(rmux_bridge_path, &sb) == 0 &&
	    sb.st_dev == rmux_bridge_socket_dev &&
	    sb.st_ino == rmux_bridge_socket_ino)
		unlink(rmux_bridge_path);
	rmux_bridge_enabled = 0;
	rmux_bridge_path[0] = '\0';
}

void
rmux_bridge_pane_created(struct window_pane *wp)
{
	if (!rmux_bridge_enabled)
		return;
	wp->rmux_pty_generation = 0;
	wp->rmux_screen_generation = 1;
	wp->rmux_generation_exhausted = 0;
	if (rmux_bridge_bump(&rmux_bridge_revision) != 0)
		rmux_bridge_revision_exhausted = wp->rmux_generation_exhausted = 1;
}

void
rmux_bridge_pane_destroyed(struct window_pane *wp)
{
	if (!rmux_bridge_enabled)
		return;
	if (rmux_bridge_bump(&rmux_bridge_revision) != 0)
		rmux_bridge_revision_exhausted = wp->rmux_generation_exhausted = 1;
}

void
rmux_bridge_pane_state_changed(struct window_pane *wp)
{
	if (!rmux_bridge_enabled)
		return;
	if (rmux_bridge_bump(&rmux_bridge_revision) != 0)
		rmux_bridge_revision_exhausted = wp->rmux_generation_exhausted = 1;
}

void
rmux_bridge_pty_changed(struct window_pane *wp)
{
	if (!rmux_bridge_enabled)
		return;
	if (rmux_bridge_bump(&wp->rmux_pty_generation) != 0 ||
	    rmux_bridge_bump(&wp->rmux_screen_generation) != 0)
		wp->rmux_generation_exhausted = 1;
	if (rmux_bridge_bump(&rmux_bridge_revision) != 0)
		rmux_bridge_revision_exhausted = wp->rmux_generation_exhausted = 1;
}

void
rmux_bridge_output_changed(struct window_pane *wp)
{
	if (!rmux_bridge_enabled)
		return;
	if (rmux_bridge_bump(&wp->rmux_screen_generation) != 0)
		wp->rmux_generation_exhausted = 1;
}

void
rmux_bridge_geometry_changed(struct window_pane *wp)
{
	if (!rmux_bridge_enabled)
		return;
	if (rmux_bridge_bump(&wp->rmux_screen_generation) != 0)
		wp->rmux_generation_exhausted = 1;
	if (rmux_bridge_bump(&rmux_bridge_revision) != 0)
		rmux_bridge_revision_exhausted = wp->rmux_generation_exhausted = 1;
}
