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

#include <netinet/in.h>

#include <resolv.h>
#include <stdarg.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>

#include "tmux.h"
#include "masil-bridge.h"
#include "yyjson.h"

/* masil: C1a keeps retained receipts in exactly 2,048 fixed 256-byte slots. */
#define MASIL_ACTION_SLOTS		2048
#define MASIL_ACTION_SLOT_BYTES	256
/* masil: store the SHA-256 digest as bytes to leave launch receipt room. */
#define MASIL_ACTION_PAYLOAD_DIGEST_BYTES 32
/* C3 keeps a 158-byte operation key plus launch result in each 256 B slot. */
#define MASIL_ACTION_KEY_BYTES		183
#define MASIL_ACTION_CLOSED_EPOCHS	8
#define MASIL_ACTION_CLOSED_EPOCH_CAPACITY \
	(MASIL_ACTION_SLOTS + MASIL_ACTION_CLOSED_EPOCHS)
#define MASIL_ACTION_MAX_KEYS		64
#define MASIL_ACTION_LIST_ENTRY_BYTES	4096
#define MASIL_ACTION_EPOCH_ENTRY_BYTES	128
#define MASIL_ACTION_LIST_TRAILER_BYTES	128
#define MASIL_ACTION_STAGING_SLOTS	4
#define MASIL_ACTION_STAGING_BYTES	(32 * 1024)
/* data_b64 is at most 6 KiB so a chunk request fits the 8 KiB bridge frame. */
#define MASIL_ACTION_STAGING_CHUNK_B64	(6 * 1024)
#define MASIL_ACTION_STAGING_CHUNK_BYTES	(MASIL_ACTION_STAGING_CHUNK_B64 / 4 * 3)
#define MASIL_ACTION_STAGING_ID_BYTES	64
#define MASIL_ACTION_STAGING_TTL	30000
#define MASIL_ACTION_LAUNCH_MAX_ARGV	256
#define MASIL_ACTION_LAUNCH_MAX_ENV	64
#define MASIL_ACTION_LAUNCH_BYTES	(32 * 1024)
/* masil: D1a answers list this many affected clients and flag the rest. */
#define MASIL_ACTION_FOCUS_AFFECTED	64
#define MASIL_ACTION_FOCUS_CLIENT_ID	96

enum masil_action_result {
	MASIL_ACTION_RESULT_NONE,
	MASIL_ACTION_RESULT_APPLIED,
	MASIL_ACTION_RESULT_REJECTED,
	MASIL_ACTION_RESULT_UNSUPPORTED,
	MASIL_ACTION_RESULT_NOT_APPLIED
};

enum masil_action_reason {
	MASIL_ACTION_REASON_NONE,
	MASIL_ACTION_REASON_CORE_BOOT_CHANGED,
	MASIL_ACTION_REASON_TARGET_GONE,
	MASIL_ACTION_REASON_PTY_GENERATION_CHANGED,
	MASIL_ACTION_REASON_FOREGROUND_PGID_CHANGED,
	MASIL_ACTION_REASON_CURRENT_COMMAND_CHANGED,
	MASIL_ACTION_REASON_META_CHANGED,
	MASIL_ACTION_REASON_PANE_DEAD,
	MASIL_ACTION_REASON_INPUT_OFF,
	MASIL_ACTION_REASON_SYNCHRONIZE_PANES,
	MASIL_ACTION_REASON_MODE_ACTIVE,
	MASIL_ACTION_REASON_OUTPUT_CHANGED,
	MASIL_ACTION_REASON_TRACKED_CHANGED,
	MASIL_ACTION_REASON_TITLE_CHANGED,
	MASIL_ACTION_REASON_PROGRESS_CHANGED,
	MASIL_ACTION_REASON_INVALID_KEY,
	MASIL_ACTION_REASON_WINDOW_UNLINKED,
	MASIL_ACTION_REASON_STAGING_INCOMPLETE,
	MASIL_ACTION_REASON_STAGING_DIGEST_MISMATCH,
	MASIL_ACTION_REASON_STAGING_KEY_MISMATCH,
	MASIL_ACTION_REASON_BRACKET_MODE_OFF,
	MASIL_ACTION_REASON_LAUNCH_FAILED,
	MASIL_ACTION_REASON_CLIENT_ABSENT,
	MASIL_ACTION_REASON_CLIENT_READONLY,
	MASIL_ACTION_REASON_VIEW_CHANGED,
	MASIL_ACTION_REASON_ZOOM_CHANGED,
	MASIL_ACTION_REASON_SHARED_FOCUS_CONFLICT
};

enum masil_action_kind {
	MASIL_ACTION_KEYS,
	MASIL_ACTION_KILL_PANE,
	MASIL_ACTION_INPUT_COMMIT,
	MASIL_ACTION_LAUNCH,
	MASIL_ACTION_FOCUS,
	MASIL_ACTION_UNKNOWN
};

struct masil_action_slot {
	uint64_t	 epoch;
	uint64_t	 seq;
	uint64_t	 pty_generation;
	uint32_t	 pane_id;
	uint32_t	 pid;
	uint32_t	 queued_bytes;
	unsigned char	 payload_digest[MASIL_ACTION_PAYLOAD_DIGEST_BYTES];
	uint8_t		 used;
	uint8_t		 result;
	uint8_t		 reason;
	uint8_t		 key_length;
	uint8_t		 launch;
	unsigned char	 key[MASIL_ACTION_KEY_BYTES];
};
typedef char masil_action_slot_size_must_be_256[
    sizeof(struct masil_action_slot) == MASIL_ACTION_SLOT_BYTES ? 1 : -1];

struct masil_action_closed_epoch {
	uint64_t	 epoch;
	uint64_t	 watermark;
	size_t		 slots_used;
};

struct masil_action_key {
	unsigned char	 bytes[MASIL_ACTION_KEY_BYTES];
	size_t		 length;
};

struct masil_action_ticket {
	uint64_t	 epoch;
	uint64_t	 seq;
};

struct masil_action_target {
	const char	*core_boot_id;
	size_t		 core_boot_id_length;
	const char	*pane_id;
	size_t		 pane_id_length;
	uint64_t	 pty_generation;
};

struct masil_action_preconditions {
	const char	*foreground_pgid;
	size_t		 foreground_pgid_length;
	const char	*current_command;
	size_t		 current_command_length;
	unsigned char	 meta_digest[32];
	int		 pane_dead;
	int		 input_off;
	int		 synchronize;
	int		 has_mode;
	int		 mode_none;
	int		 has_output_generation;
	uint64_t	 output_generation;
	int		 has_tracked_digest;
	unsigned char	 tracked_digest[32];
	const char	*title;
	size_t		 title_length;
	int		 has_title;
	const char	*progress;
	size_t		 progress_length;
	int		 has_progress;
};

enum masil_action_launch_mode {
	MASIL_ACTION_LAUNCH_WINDOW,
	MASIL_ACTION_LAUNCH_SPLIT
};

struct masil_action_data {
	enum masil_action_kind kind;
	yyjson_val		*keys;
	const char		*staging_id;
	size_t			 staging_id_length;
	int			 bracketed;
	int			 submit_enter;
	int			 input_legacy;
	enum masil_action_launch_mode launch_mode;
	const char		*launch_target;
	size_t			 launch_target_length;
	const char		*launch_name;
	size_t			 launch_name_length;
	int			 launch_has_name;
	const char		*launch_cwd;
	size_t			 launch_cwd_length;
	uint64_t		 launch_cwd_dev;
	uint64_t		 launch_cwd_ino;
	yyjson_val		*launch_env;
	yyjson_val		*launch_argv;
	const char		*launch_spec_staging_id;
	size_t			 launch_spec_staging_id_length;
	int			 launch_staged;
	/* masil: D1a focus; an argument-free focus stays an unsupported stub. */
	int			 focus_args;
	const char		*focus_client_id;
	size_t			 focus_client_id_length;
	uint64_t		 focus_view_revision;
	int			 focus_shared;
	int			 focus_restore_zoom;
	u_int			 focus_zoom_window;
	u_int			 focus_zoom_pane;
	uint64_t		 focus_zoom_generation;
};

struct masil_action_request {
	struct masil_action_key		 operation_key;
	struct masil_action_ticket	 ticket;
	unsigned char			 payload_digest[MASIL_ACTION_PAYLOAD_DIGEST_BYTES];
	int				 retain;
	struct masil_action_target	 target;
	struct masil_action_preconditions preconditions;
	struct masil_action_data		 action;
};

struct masil_action_outcome {
	uint32_t	 queued_bytes;
	u_int		 pane_id;
	pid_t		 pid;
	uint64_t	 pty_generation;
	int		 launch;
	/* masil: D1a focus answers. */
	int		 focus_applied;
	int		 focus_affected_listed;
	uint64_t	 focus_view_revision;
	size_t		 focus_affected_count;
	struct {
		uint64_t serial;
		int	 stalled;
	}		 focus_affected[MASIL_ACTION_FOCUS_AFFECTED];
};

/* masil: C3 context survives its cmdq item until the child status pipe EOF. */
struct masil_launch_context {
	int		 references;
	char		*cwd;
	char		*name;
	struct environ	*environ;
	int		 argc;
	char		**argv;
	uint64_t	 cwd_dev;
	uint64_t	 cwd_ino;
	u_int		 pane_id;
	pid_t		 pid;
	uint64_t	 pty_generation;
	char		*error;
	int		 status_fd;
	struct event	 status_event;
	struct masil_launch_status status;
	size_t		 status_used;
	int		 pane_created;
	int		 command_started;
	int		 cancelled;
};

struct masil_action_json {
	char	*buf;
	size_t	 len;
	size_t	 cap;
	int	 failed;
};

struct masil_action_sha256_ctx {
	uint32_t state[8];
	uint64_t bits;
	unsigned char block[64];
	size_t	 used;
};

struct masil_action_staging_slot {
	struct masil_action_key		 operation_key;
	struct masil_action_sha256_ctx sha256;
	unsigned char			 expected_digest[32];
	char				 id[MASIL_ACTION_STAGING_ID_BYTES + 1];
	char				*data;
	uint64_t			 deadline;
	size_t				 total_len;
	size_t				 received;
	int				 active;
};

static struct masil_action_slot *masil_action_slots;
static size_t masil_action_slots_used;
static size_t masil_action_slot_cursor;
static uint64_t masil_action_next_epoch;
static uint64_t masil_action_active_epoch;
static uint64_t masil_action_active_next_seq;
static size_t masil_action_active_slots_used;
static struct masil_action_closed_epoch *masil_action_closed_epochs;
static size_t masil_action_closed_count;
static struct masil_action_staging_slot
    masil_action_staging[MASIL_ACTION_STAGING_SLOTS];
static struct event masil_action_staging_event;
static int masil_action_staging_timer_active;

static void printflike(2, 3) masil_action_json_printf(
    struct masil_action_json *, const char *, ...);

static uint32_t
masil_action_rotr(uint32_t value, u_int bits)
{
	return ((value >> bits) | (value << (32 - bits)));
}

static void
masil_action_sha256_transform(struct masil_action_sha256_ctx *ctx,
    const unsigned char block[64])
{
	static const uint32_t constants[64] = {
		0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5,
		0x3956c25b, 0x59f111f1, 0x923f82a4, 0xab1c5ed5,
		0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3,
		0x72be5d74, 0x80deb1fe, 0x9bdc06a7, 0xc19bf174,
		0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc,
		0x2de92c6f, 0x4a7484aa, 0x5cb0a9dc, 0x76f988da,
		0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7,
		0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967,
		0x27b70a85, 0x2e1b2138, 0x4d2c6dfc, 0x53380d13,
		0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85,
		0xa2bfe8a1, 0xa81a664b, 0xc24b8b70, 0xc76c51a3,
		0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070,
		0x19a4c116, 0x1e376c08, 0x2748774c, 0x34b0bcb5,
		0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
		0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208,
		0x90befffa, 0xa4506ceb, 0xbef9a3f7, 0xc67178f2
	};
	uint32_t work[64];
	uint32_t a, b, c, d, e, f, g, h, s0, s1, choice, majority;
	uint32_t temporary1, temporary2;
	u_int i;

	for (i = 0; i < 16; i++) {
		work[i] = ((uint32_t)block[i * 4] << 24) |
		    ((uint32_t)block[i * 4 + 1] << 16) |
		    ((uint32_t)block[i * 4 + 2] << 8) |
		    (uint32_t)block[i * 4 + 3];
	}
	for (i = 16; i < nitems(work); i++) {
		s0 = masil_action_rotr(work[i - 15], 7) ^
		    masil_action_rotr(work[i - 15], 18) ^ (work[i - 15] >> 3);
		s1 = masil_action_rotr(work[i - 2], 17) ^
		    masil_action_rotr(work[i - 2], 19) ^ (work[i - 2] >> 10);
		work[i] = work[i - 16] + s0 + work[i - 7] + s1;
	}
	a = ctx->state[0];
	b = ctx->state[1];
	c = ctx->state[2];
	d = ctx->state[3];
	e = ctx->state[4];
	f = ctx->state[5];
	g = ctx->state[6];
	h = ctx->state[7];
	for (i = 0; i < nitems(work); i++) {
		s1 = masil_action_rotr(e, 6) ^ masil_action_rotr(e, 11) ^
		    masil_action_rotr(e, 25);
		choice = (e & f) ^ ((~e) & g);
		temporary1 = h + s1 + choice + constants[i] + work[i];
		s0 = masil_action_rotr(a, 2) ^ masil_action_rotr(a, 13) ^
		    masil_action_rotr(a, 22);
		majority = (a & b) ^ (a & c) ^ (b & c);
		temporary2 = s0 + majority;
		h = g;
		g = f;
		f = e;
		e = d + temporary1;
		d = c;
		c = b;
		b = a;
		a = temporary1 + temporary2;
	}
	ctx->state[0] += a;
	ctx->state[1] += b;
	ctx->state[2] += c;
	ctx->state[3] += d;
	ctx->state[4] += e;
	ctx->state[5] += f;
	ctx->state[6] += g;
	ctx->state[7] += h;
}

static void
masil_action_sha256_init(struct masil_action_sha256_ctx *ctx)
{
	static const uint32_t initial_state[8] = {
		0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a,
		0x510e527f, 0x9b05688c, 0x1f83d9ab, 0x5be0cd19
	};

	memset(ctx, 0, sizeof *ctx);
	memcpy(ctx->state, initial_state, sizeof ctx->state);
}

static void
masil_action_sha256_update(struct masil_action_sha256_ctx *ctx,
    const unsigned char *data, size_t length)
{
	size_t copy;

	ctx->bits += (uint64_t)length * 8;
	while (length != 0) {
		copy = sizeof ctx->block - ctx->used;
		if (copy > length)
			copy = length;
		memcpy(ctx->block + ctx->used, data, copy);
		ctx->used += copy;
		data += copy;
		length -= copy;
		if (ctx->used == sizeof ctx->block) {
			masil_action_sha256_transform(ctx, ctx->block);
			ctx->used = 0;
		}
	}
}

static void
masil_action_sha256_final(struct masil_action_sha256_ctx *ctx,
    unsigned char digest[32])
{
	uint64_t bits = ctx->bits;
	u_int i;

	ctx->block[ctx->used++] = 0x80;
	if (ctx->used > 56) {
		while (ctx->used < sizeof ctx->block)
			ctx->block[ctx->used++] = 0;
		masil_action_sha256_transform(ctx, ctx->block);
		ctx->used = 0;
	}
	while (ctx->used < 56)
		ctx->block[ctx->used++] = 0;
	for (i = 0; i < 8; i++)
		ctx->block[63 - i] = (unsigned char)(bits >> (i * 8));
	masil_action_sha256_transform(ctx, ctx->block);
	for (i = 0; i < nitems(ctx->state); i++) {
		digest[i * 4] = (unsigned char)(ctx->state[i] >> 24);
		digest[i * 4 + 1] = (unsigned char)(ctx->state[i] >> 16);
		digest[i * 4 + 2] = (unsigned char)(ctx->state[i] >> 8);
		digest[i * 4 + 3] = (unsigned char)ctx->state[i];
	}
}

/* masil: exported so focused C tests can verify the dependency-free digest. */
void
masil_action_sha256(const void *data, size_t length, unsigned char digest[32])
{
	struct masil_action_sha256_ctx ctx;

	masil_action_sha256_init(&ctx);
	masil_action_sha256_update(&ctx, data, length);
	masil_action_sha256_final(&ctx, digest);
}

static void
masil_action_digest_hex(const unsigned char digest[32], char out[65])
{
	static const char hex[] = "0123456789abcdef";
	size_t i;

	for (i = 0; i < 32; i++) {
		out[i * 2] = hex[digest[i] >> 4];
		out[i * 2 + 1] = hex[digest[i] & 0x0f];
	}
	out[64] = '\0';
}

static int
masil_action_digest_decode(yyjson_val *value, unsigned char digest[32])
{
	const char *string;
	size_t length, i;
	unsigned char high, low;

	if (!yyjson_is_str(value) || yyjson_get_len(value) != 64)
		return (-1);
	string = yyjson_get_str(value);
	length = yyjson_get_len(value);
	for (i = 0; i < length; i += 2) {
		if (string[i] >= '0' && string[i] <= '9')
			high = string[i] - '0';
		else if (string[i] >= 'a' && string[i] <= 'f')
			high = string[i] - 'a' + 10;
		else
			return (-1);
		if (string[i + 1] >= '0' && string[i + 1] <= '9')
			low = string[i + 1] - '0';
		else if (string[i + 1] >= 'a' && string[i + 1] <= 'f')
			low = string[i + 1] - 'a' + 10;
		else
			return (-1);
		digest[i / 2] = (high << 4) | low;
	}
	return (0);
}

/* masil: payload digest JSON is SHA-256 hex; receipts store its 32 bytes. */
static int
masil_action_parse_payload_digest(yyjson_val *value,
    unsigned char digest[MASIL_ACTION_PAYLOAD_DIGEST_BYTES])
{
	return (masil_action_digest_decode(value, digest));
}

static void
masil_action_json_putn(struct masil_action_json *json, const char *value,
    size_t length)
{
	if (json->failed)
		return;
	if (length > json->cap - json->len) {
		json->failed = 1;
		return;
	}
	memcpy(json->buf + json->len, value, length);
	json->len += length;
}

static void
masil_action_json_puts(struct masil_action_json *json, const char *value)
{
	masil_action_json_putn(json, value, strlen(value));
}

static void
masil_action_json_printf(struct masil_action_json *json, const char *fmt, ...)
{
	va_list ap;
	int length;

	if (json->failed)
		return;
	va_start(ap, fmt);
	length = vsnprintf(json->buf + json->len, json->cap - json->len,
	    fmt, ap);
	va_end(ap);
	if (length < 0 || (size_t)length >= json->cap - json->len) {
		json->failed = 1;
		return;
	}
	json->len += length;
}

static void
masil_action_json_quote(struct masil_action_json *json, const char *value,
    size_t length)
{
	static const char hex[] = "0123456789abcdef";
	char escaped[6];
	unsigned char ch;
	size_t i;

	masil_action_json_puts(json, "\"");
	for (i = 0; i < length; i++) {
		ch = value[i];
		switch (ch) {
		case '\"':
			masil_action_json_puts(json, "\\\"");
			break;
		case '\\':
			masil_action_json_puts(json, "\\\\");
			break;
		case '\b':
			masil_action_json_puts(json, "\\b");
			break;
		case '\f':
			masil_action_json_puts(json, "\\f");
			break;
		case '\n':
			masil_action_json_puts(json, "\\n");
			break;
		case '\r':
			masil_action_json_puts(json, "\\r");
			break;
		case '\t':
			masil_action_json_puts(json, "\\t");
			break;
		default:
			if (ch < 0x20) {
				escaped[0] = '\\';
				escaped[1] = 'u';
				escaped[2] = '0';
				escaped[3] = '0';
				escaped[4] = hex[ch >> 4];
				escaped[5] = hex[ch & 0x0f];
				masil_action_json_putn(json, escaped, sizeof escaped);
			} else
				masil_action_json_putn(json, (const char *)&value[i], 1);
			break;
		}
	}
	masil_action_json_puts(json, "\"");
}

static int
masil_action_json_done(struct masil_action_json *json)
{
	if (json->failed || json->len == 0 || json->len >= json->cap)
		return (-1);
	json->buf[json->len] = '\0';
	return (0);
}

static int
masil_action_response_error(char *response, size_t response_size,
    const char *request_id, size_t request_id_length, const char *code)
{
	struct masil_action_json json = { response, 0, response_size, 0 };

	masil_action_json_puts(&json, "{\"v\":1,\"kind\":\"error\",\"request_id\":");
	masil_action_json_quote(&json, request_id, request_id_length);
	masil_action_json_puts(&json, ",\"code\":");
	masil_action_json_quote(&json, code, strlen(code));
	masil_action_json_puts(&json, ",\"message\":");
	masil_action_json_quote(&json, code, strlen(code));
	masil_action_json_puts(&json, "}");
	return (masil_action_json_done(&json) == 0 ? 1 : -1);
}

static const char *
masil_action_reason_name(enum masil_action_reason reason)
{
	switch (reason) {
	case MASIL_ACTION_REASON_CORE_BOOT_CHANGED:
		return ("core_boot_changed");
	case MASIL_ACTION_REASON_TARGET_GONE:
		return ("target_gone");
	case MASIL_ACTION_REASON_PTY_GENERATION_CHANGED:
		return ("pty_generation_changed");
	case MASIL_ACTION_REASON_FOREGROUND_PGID_CHANGED:
		return ("foreground_pgid_changed");
	case MASIL_ACTION_REASON_CURRENT_COMMAND_CHANGED:
		return ("current_command_changed");
	case MASIL_ACTION_REASON_META_CHANGED:
		return ("meta_changed");
	case MASIL_ACTION_REASON_PANE_DEAD:
		return ("pane_dead");
	case MASIL_ACTION_REASON_INPUT_OFF:
		return ("input_off");
	case MASIL_ACTION_REASON_SYNCHRONIZE_PANES:
		return ("synchronize_panes");
	case MASIL_ACTION_REASON_MODE_ACTIVE:
		return ("mode_active");
	case MASIL_ACTION_REASON_OUTPUT_CHANGED:
		return ("output_changed");
	case MASIL_ACTION_REASON_TRACKED_CHANGED:
		return ("tracked_changed");
	case MASIL_ACTION_REASON_TITLE_CHANGED:
		return ("title_changed");
	case MASIL_ACTION_REASON_PROGRESS_CHANGED:
		return ("progress_changed");
	case MASIL_ACTION_REASON_INVALID_KEY:
		return ("invalid_key");
	case MASIL_ACTION_REASON_WINDOW_UNLINKED:
		return ("window_unlinked");
	case MASIL_ACTION_REASON_STAGING_INCOMPLETE:
		return ("staging_incomplete");
	case MASIL_ACTION_REASON_STAGING_DIGEST_MISMATCH:
		return ("staging_digest_mismatch");
	case MASIL_ACTION_REASON_STAGING_KEY_MISMATCH:
		return ("staging_key_mismatch");
	case MASIL_ACTION_REASON_BRACKET_MODE_OFF:
		return ("bracket_mode_off");
	case MASIL_ACTION_REASON_LAUNCH_FAILED:
		return ("launch_failed");
	case MASIL_ACTION_REASON_CLIENT_ABSENT:
		return ("client_absent");
	case MASIL_ACTION_REASON_CLIENT_READONLY:
		return ("client_readonly");
	case MASIL_ACTION_REASON_VIEW_CHANGED:
		return ("view_changed");
	case MASIL_ACTION_REASON_ZOOM_CHANGED:
		return ("zoom_changed");
	case MASIL_ACTION_REASON_SHARED_FOCUS_CONFLICT:
		return ("shared_focus_conflict");
	case MASIL_ACTION_REASON_NONE:
		break;
	}
	return ("unknown");
}

static int
masil_action_result_text(enum masil_action_result result,
    enum masil_action_reason reason, char *out, size_t out_size)
{
	const char *name;

	switch (result) {
	case MASIL_ACTION_RESULT_APPLIED:
		if (strlcpy(out, "applied", out_size) >= out_size)
			return (-1);
		return (0);
	case MASIL_ACTION_RESULT_REJECTED:
		name = masil_action_reason_name(reason);
		if (xsnprintf(out, out_size, "rejected_before_effect:%s", name) < 0)
			return (-1);
		return (0);
	case MASIL_ACTION_RESULT_UNSUPPORTED:
		if (strlcpy(out, "unsupported_action", out_size) >= out_size)
			return (-1);
		return (0);
	case MASIL_ACTION_RESULT_NOT_APPLIED:
		if (strlcpy(out, "not_applied", out_size) >= out_size)
			return (-1);
		return (0);
	case MASIL_ACTION_RESULT_NONE:
		break;
	}
	return (-1);
}

static int
masil_action_response_result(char *response, size_t response_size,
    const char *kind, const char *request_id, size_t request_id_length,
    const struct masil_action_ticket *ticket, enum masil_action_result result,
    enum masil_action_reason reason,
    const struct masil_action_outcome *outcome)
{
	struct masil_action_json json = { response, 0, response_size, 0 };
	unsigned char digest[32];
	char result_text[128], digest_text[65];
	size_t i;

	if (masil_action_result_text(result, reason, result_text,
	    sizeof result_text) != 0)
		return (-1);
	masil_action_sha256(result_text, strlen(result_text), digest);
	masil_action_digest_hex(digest, digest_text);
	masil_action_json_puts(&json, "{\"v\":1,\"kind\":");
	masil_action_json_quote(&json, kind, strlen(kind));
	masil_action_json_puts(&json, ",\"request_id\":");
	masil_action_json_quote(&json, request_id, request_id_length);
	masil_action_json_printf(&json,
	    ",\"dispatch_ticket\":{\"epoch\":\"%llu\",\"seq\":\"%llu\"}",
	    (unsigned long long)ticket->epoch,
	    (unsigned long long)ticket->seq);
	masil_action_json_puts(&json, ",\"result\":");
	masil_action_json_quote(&json, result_text, strlen(result_text));
	masil_action_json_puts(&json, ",\"result_digest\":");
	masil_action_json_quote(&json, digest_text, strlen(digest_text));
	if (outcome->queued_bytes != 0)
		masil_action_json_printf(&json, ",\"queued_bytes\":%u",
		    outcome->queued_bytes);
	if (outcome->launch) {
		masil_action_json_printf(&json,
		    ",\"pane_id\":\"%%%u\",\"pid\":\"%ld\","
		    "\"pty_generation\":\"%llu\"", outcome->pane_id,
		    (long)outcome->pid,
		    (unsigned long long)outcome->pty_generation);
	}
	if (outcome->focus_applied)
		masil_action_json_printf(&json,
		    ",\"phase\":\"logical_selection_applied\","
		    "\"view_revision\":\"%llu\"",
		    (unsigned long long)outcome->focus_view_revision);
	if (outcome->focus_affected_listed) {
		masil_action_json_puts(&json, ",\"affected\":[");
		for (i = 0; i < outcome->focus_affected_count &&
		    i < MASIL_ACTION_FOCUS_AFFECTED; i++)
			masil_action_json_printf(&json,
			    "%s{\"client_id\":\"%s:%llu\",\"stalled\":%s}",
			    i == 0 ? "" : ",", masil_bridge_get_boot_id(),
			    (unsigned long long)outcome->focus_affected[i].serial,
			    outcome->focus_affected[i].stalled ? "true" : "false");
		masil_action_json_puts(&json, "]");
		if (outcome->focus_affected_count > MASIL_ACTION_FOCUS_AFFECTED)
			masil_action_json_puts(&json, ",\"affected_truncated\":true");
	}
	masil_action_json_puts(&json, "}");
	return (masil_action_json_done(&json));
}

static int
masil_action_key_allowed(const char *key, size_t length,
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
masil_action_schema(yyjson_val *value, const char *const *allowed,
    size_t allowed_count)
{
	yyjson_obj_iter iter;
	yyjson_val *key;
	const char *seen[32], *name;
	size_t seen_lengths[32], count = 0, length, i;

	if (!yyjson_is_obj(value) || yyjson_obj_size(value) > nitems(seen))
		return (-1);
	iter = yyjson_obj_iter_with(value);
	while ((key = yyjson_obj_iter_next(&iter)) != NULL) {
		name = yyjson_get_str(key);
		length = yyjson_get_len(key);
		for (i = 0; i < count; i++) {
			if (seen_lengths[i] == length &&
			    memcmp(seen[i], name, length) == 0)
				return (-1);
		}
		if (!masil_action_key_allowed(name, length, allowed, allowed_count))
			return (-1);
		seen[count] = name;
		seen_lengths[count++] = length;
	}
	return (0);
}

static int
masil_action_parse_u64(yyjson_val *value, uint64_t *result)
{
	const char *string;
	size_t length, i;
	uint64_t number, digit;

	if (yyjson_is_uint(value)) {
		*result = yyjson_get_uint(value);
		return (0);
	}
	if (!yyjson_is_str(value))
		return (-1);
	string = yyjson_get_str(value);
	length = yyjson_get_len(value);
	if (length == 0 || length > 20)
		return (-1);
	number = 0;
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
masil_action_parse_string(yyjson_val *object, const char *name,
    const char **string, size_t *length)
{
	yyjson_val *value = yyjson_obj_get(object, name);

	if (!yyjson_is_str(value))
		return (-1);
	*string = yyjson_get_str(value);
	*length = yyjson_get_len(value);
	return (0);
}

static int
masil_action_parse_core_boot_id(yyjson_val *object, const char **boot_id,
    size_t *boot_id_length)
{
	return (masil_action_parse_string(object, "core_boot_id", boot_id,
	    boot_id_length) == 0 && *boot_id_length != 0 ? 0 : -1);
}

/* The operation key components, in their stored order. */
static const char *const masil_action_key_fields[] = {
	"environment_id", "principal", "namespace_epoch", "namespace_nonce",
	"operation_id"
};

static int
masil_action_parse_operation_key(yyjson_val *value,
    struct masil_action_key *operation_key)
{
	const char *string;
	size_t length, i, used = 0;

	if (masil_action_schema(value, masil_action_key_fields,
	    nitems(masil_action_key_fields)) != 0)
		return (-1);
	for (i = 0; i < nitems(masil_action_key_fields); i++) {
		if (masil_action_parse_string(value, masil_action_key_fields[i],
		    &string, &length) != 0 ||
		    length == 0 || length > UINT8_MAX ||
		    used + 1 + length > sizeof operation_key->bytes)
			return (-1);
		operation_key->bytes[used++] = (unsigned char)length;
		memcpy(operation_key->bytes + used, string, length);
		used += length;
	}
	operation_key->length = used;
	return (0);
}

static int
masil_action_parse_ticket(yyjson_val *value, struct masil_action_ticket *ticket)
{
	static const char *const fields[] = { "epoch", "seq" };

	if (masil_action_schema(value, fields, nitems(fields)) != 0 ||
	    masil_action_parse_u64(yyjson_obj_get(value, "epoch"),
	    &ticket->epoch) != 0 ||
	    masil_action_parse_u64(yyjson_obj_get(value, "seq"),
	    &ticket->seq) != 0 || ticket->epoch == 0)
		return (-1);
	return (0);
}

static int
masil_action_parse_target(yyjson_val *value, struct masil_action_target *target)
{
	static const char *const fields[] = {
		"core_boot_id", "pane_id", "pty_generation"
	};

	if (masil_action_schema(value, fields, nitems(fields)) != 0 ||
	    masil_action_parse_string(value, "core_boot_id", &target->core_boot_id,
	    &target->core_boot_id_length) != 0 ||
	    masil_action_parse_string(value, "pane_id", &target->pane_id,
	    &target->pane_id_length) != 0 ||
	    masil_action_parse_u64(yyjson_obj_get(value, "pty_generation"),
	    &target->pty_generation) != 0 || target->core_boot_id_length == 0 ||
	    target->pane_id_length < 2 || target->pane_id_length > 32 ||
	    memchr(target->pane_id, '\0', target->pane_id_length) != NULL)
		return (-1);
	return (0);
}

/* masil: launch has no agent-pane target when it creates a new window. */
static int
masil_action_parse_launch_target(yyjson_val *value,
    const struct masil_action_data *action, struct masil_action_target *target)
{
	static const char *const window_fields[] = { "core_boot_id" };

	if (action->launch_mode != MASIL_ACTION_LAUNCH_WINDOW)
		return (masil_action_parse_target(value, target));
	if (masil_action_schema(value, window_fields, nitems(window_fields)) != 0 ||
	    masil_action_parse_string(value, "core_boot_id", &target->core_boot_id,
	    &target->core_boot_id_length) != 0 || target->core_boot_id_length == 0)
		return (-1);
	return (0);
}

static int
masil_action_launch_preconditions_valid(yyjson_val *value)
{
	return (yyjson_is_obj(value) && yyjson_obj_size(value) == 0 ? 0 : -1);
}

/* masil: focus checks only the target pane, so pane_dead is its sole entry. */
static int
masil_action_parse_focus_preconditions(yyjson_val *value,
    struct masil_action_preconditions *preconditions)
{
	static const char *const fields[] = { "pane_dead" };
	yyjson_val *item;

	if (masil_action_schema(value, fields, nitems(fields)) != 0)
		return (-1);
	item = yyjson_obj_get(value, "pane_dead");
	if (!yyjson_is_bool(item))
		return (-1);
	preconditions->pane_dead = yyjson_get_bool(item);
	return (0);
}

static int
masil_action_parse_preconditions(yyjson_val *value,
    struct masil_action_preconditions *preconditions)
{
	static const char *const fields[] = {
		"expected_foreground_pgid", "expected_current_command",
		"meta_digest", "pane_dead", "input_off", "synchronize", "mode",
		"expected_output_generation", "tracked_digest", "expected_title",
		"expected_progress"
	};
	yyjson_val *item;
	const char *string;
	size_t length;

	if (masil_action_schema(value, fields, nitems(fields)) != 0 ||
	    masil_action_parse_string(value, "expected_foreground_pgid",
	    &preconditions->foreground_pgid,
	    &preconditions->foreground_pgid_length) != 0 ||
	    masil_action_parse_string(value, "expected_current_command",
	    &preconditions->current_command,
	    &preconditions->current_command_length) != 0 ||
	    masil_action_digest_decode(yyjson_obj_get(value, "meta_digest"),
	    preconditions->meta_digest) != 0)
		return (-1);
	item = yyjson_obj_get(value, "pane_dead");
	if (!yyjson_is_bool(item))
		return (-1);
	preconditions->pane_dead = yyjson_get_bool(item);
	item = yyjson_obj_get(value, "input_off");
	if (!yyjson_is_bool(item))
		return (-1);
	preconditions->input_off = yyjson_get_bool(item);
	item = yyjson_obj_get(value, "synchronize");
	if (!yyjson_is_bool(item))
		return (-1);
	preconditions->synchronize = yyjson_get_bool(item);
	item = yyjson_obj_get(value, "mode");
	if (item != NULL) {
		if (!yyjson_is_str(item))
			return (-1);
		string = yyjson_get_str(item);
		length = yyjson_get_len(item);
		if (length != 4 || memcmp(string, "none", 4) != 0)
			return (-1);
		preconditions->mode_none = 1;
		preconditions->has_mode = 1;
	}
	item = yyjson_obj_get(value, "expected_output_generation");
	if (item != NULL) {
		if (masil_action_parse_u64(item, &preconditions->output_generation) != 0)
			return (-1);
		preconditions->has_output_generation = 1;
	}
	item = yyjson_obj_get(value, "tracked_digest");
	if (item != NULL) {
		if (masil_action_digest_decode(item, preconditions->tracked_digest) != 0)
			return (-1);
		preconditions->has_tracked_digest = 1;
	}
	item = yyjson_obj_get(value, "expected_title");
	if (item != NULL) {
		if (!yyjson_is_str(item))
			return (-1);
		preconditions->title = yyjson_get_str(item);
		preconditions->title_length = yyjson_get_len(item);
		preconditions->has_title = 1;
	}
	item = yyjson_obj_get(value, "expected_progress");
	if (item != NULL) {
		if (!yyjson_is_str(item))
			return (-1);
		preconditions->progress = yyjson_get_str(item);
		preconditions->progress_length = yyjson_get_len(item);
		preconditions->has_progress = 1;
	}
	return (0);
}

static enum masil_action_kind
masil_action_kind_from_string(const char *string, size_t length)
{
	if (length == 4 && memcmp(string, "keys", 4) == 0)
		return (MASIL_ACTION_KEYS);
	if (length == 9 && memcmp(string, "kill_pane", 9) == 0)
		return (MASIL_ACTION_KILL_PANE);
	if (length == 12 && memcmp(string, "input_commit", 12) == 0)
		return (MASIL_ACTION_INPUT_COMMIT);
	if (length == 6 && memcmp(string, "launch", 6) == 0)
		return (MASIL_ACTION_LAUNCH);
	if (length == 5 && memcmp(string, "focus", 5) == 0)
		return (MASIL_ACTION_FOCUS);
	return (MASIL_ACTION_UNKNOWN);
}

static int
masil_action_staging_id_valid(const char *id, size_t length)
{
	size_t i;

	if (length == 0 || length > MASIL_ACTION_STAGING_ID_BYTES)
		return (0);
	for (i = 0; i < length; i++) {
		if ((id[i] >= 'A' && id[i] <= 'Z') ||
		    (id[i] >= 'a' && id[i] <= 'z') ||
		    (id[i] >= '0' && id[i] <= '9') || id[i] == '.' ||
		    id[i] == '_' || id[i] == ':' || id[i] == '-')
			continue;
		return (0);
	}
	return (1);
}

static int
masil_action_parse_launch_values(yyjson_val *env, yyjson_val *argv)
{
	yyjson_arr_iter	 iter;
	yyjson_val		 *item, *key, *entry;
	const char		 *string;
	size_t			 bytes = 0, length, i;

	if (!yyjson_is_arr(argv) || yyjson_arr_size(argv) == 0 ||
	    yyjson_arr_size(argv) > MASIL_ACTION_LAUNCH_MAX_ARGV ||
	    !yyjson_is_arr(env) || yyjson_arr_size(env) > MASIL_ACTION_LAUNCH_MAX_ENV)
		return (-1);
	iter = yyjson_arr_iter_with(argv);
	for (i = 0; (item = yyjson_arr_iter_next(&iter)) != NULL; i++) {
		if (!yyjson_is_str(item))
			return (-1);
		string = yyjson_get_str(item);
		length = yyjson_get_len(item);
		if ((i == 0 && length == 0) || memchr(string, '\0', length) != NULL ||
		    length > MASIL_ACTION_LAUNCH_BYTES - bytes)
			return (-1);
		bytes += length;
	}
	iter = yyjson_arr_iter_with(env);
	while ((item = yyjson_arr_iter_next(&iter)) != NULL) {
		if (!yyjson_is_arr(item) || yyjson_arr_size(item) != 2)
			return (-1);
		key = yyjson_arr_get(item, 0);
		entry = yyjson_arr_get(item, 1);
		if (!yyjson_is_str(key) || !yyjson_is_str(entry))
			return (-1);
		string = yyjson_get_str(key);
		length = yyjson_get_len(key);
		if (length == 0 || memchr(string, '\0', length) != NULL ||
		    memchr(string, '=', length) != NULL ||
		    length > MASIL_ACTION_LAUNCH_BYTES - bytes)
			return (-1);
		bytes += length;
		string = yyjson_get_str(entry);
		length = yyjson_get_len(entry);
		if (memchr(string, '\0', length) != NULL ||
		    length > MASIL_ACTION_LAUNCH_BYTES - bytes)
			return (-1);
		bytes += length;
	}
	return (0);
}

static int
masil_action_parse_launch(struct masil_action_data *action, yyjson_val *value)
{
	static const char *const inline_fields[] = {
		"kind", "type", "mode", "target", "name", "cwd", "cwd_dev",
		"cwd_ino", "env", "argv"
	};
	static const char *const staged_fields[] = {
		"kind", "type", "mode", "target", "name", "cwd", "cwd_dev",
		"cwd_ino", "spec_staging_id"
	};
	yyjson_val	*mode, *name, *env, *argv;
	const char	*string;
	size_t		length;

	if (masil_action_schema(value, inline_fields, nitems(inline_fields)) == 0) {
		env = yyjson_obj_get(value, "env");
		argv = yyjson_obj_get(value, "argv");
		if (masil_action_parse_launch_values(env, argv) != 0)
			return (-1);
		action->launch_env = env;
		action->launch_argv = argv;
	} else if (masil_action_schema(value, staged_fields,
	    nitems(staged_fields)) == 0) {
		if (masil_action_parse_string(value, "spec_staging_id",
		    &action->launch_spec_staging_id,
		    &action->launch_spec_staging_id_length) != 0 ||
		    !masil_action_staging_id_valid(action->launch_spec_staging_id,
		    action->launch_spec_staging_id_length))
			return (-1);
		action->launch_staged = 1;
	} else
		return (-1);
	mode = yyjson_obj_get(value, "mode");
	if (!yyjson_is_str(mode))
		return (-1);
	string = yyjson_get_str(mode);
	length = yyjson_get_len(mode);
	if (length == 6 && memcmp(string, "window", length) == 0)
		action->launch_mode = MASIL_ACTION_LAUNCH_WINDOW;
	else if (length == 5 && memcmp(string, "split", length) == 0)
		action->launch_mode = MASIL_ACTION_LAUNCH_SPLIT;
	else
		return (-1);
	if (masil_action_parse_string(value, "target", &action->launch_target,
	    &action->launch_target_length) != 0 ||
	    action->launch_target_length == 0 ||
	    action->launch_target_length > 32 ||
	    memchr(action->launch_target, '\0', action->launch_target_length) != NULL ||
	    masil_action_parse_string(value, "cwd", &action->launch_cwd,
	    &action->launch_cwd_length) != 0 ||
	    action->launch_cwd_length == 0 ||
	    action->launch_cwd_length >= PATH_MAX ||
	    action->launch_cwd[0] != '/' ||
	    memchr(action->launch_cwd, '\0', action->launch_cwd_length) != NULL ||
	    masil_action_parse_u64(yyjson_obj_get(value, "cwd_dev"),
	    &action->launch_cwd_dev) != 0 ||
	    masil_action_parse_u64(yyjson_obj_get(value, "cwd_ino"),
	    &action->launch_cwd_ino) != 0)
		return (-1);
	name = yyjson_obj_get(value, "name");
	if (name != NULL) {
		if (action->launch_mode != MASIL_ACTION_LAUNCH_WINDOW ||
		    !yyjson_is_str(name))
			return (-1);
		action->launch_name = yyjson_get_str(name);
		action->launch_name_length = yyjson_get_len(name);
		if (action->launch_name_length == 0 ||
		    memchr(action->launch_name, '\0',
		    action->launch_name_length) != NULL)
			return (-1);
		action->launch_has_name = 1;
	}
	return (0);
}

static int masil_action_parse_id(const char *, size_t, char, u_int *);

/* masil: D1a focus arguments; restore_zoom names the zoomed window, pane and pty generation. */
static int
masil_action_parse_focus(struct masil_action_data *action, yyjson_val *value)
{
	static const char *const focus_fields[] = {
		"kind", "type", "client_id", "expected_view_revision", "scope",
		"restore_zoom"
	};
	static const char *const zoom_fields[] = {
		"window_id", "pane_id", "pty_generation"
	};
	yyjson_val *item;
	const char *string;
	size_t length;

	if (masil_action_schema(value, focus_fields, nitems(focus_fields)) != 0 ||
	    masil_action_parse_string(value, "client_id", &action->focus_client_id,
	    &action->focus_client_id_length) != 0 ||
	    action->focus_client_id_length == 0 ||
	    action->focus_client_id_length > MASIL_ACTION_FOCUS_CLIENT_ID ||
	    memchr(action->focus_client_id, '\0',
	    action->focus_client_id_length) != NULL ||
	    masil_action_parse_u64(yyjson_obj_get(value,
	    "expected_view_revision"), &action->focus_view_revision) != 0)
		return (-1);
	item = yyjson_obj_get(value, "scope");
	if (!yyjson_is_str(item))
		return (-1);
	string = yyjson_get_str(item);
	length = yyjson_get_len(item);
	if (length == 6 && memcmp(string, "client", length) == 0)
		action->focus_shared = 0;
	else if (length == 6 && memcmp(string, "shared", length) == 0)
		action->focus_shared = 1;
	else
		return (-1);
	item = yyjson_obj_get(value, "restore_zoom");
	if (item != NULL) {
		if (masil_action_schema(item, zoom_fields, nitems(zoom_fields)) != 0 ||
		    masil_action_parse_string(item, "window_id", &string,
		    &length) != 0 ||
		    masil_action_parse_id(string, length, '@',
		    &action->focus_zoom_window) != 0 ||
		    masil_action_parse_string(item, "pane_id", &string,
		    &length) != 0 ||
		    masil_action_parse_id(string, length, '%',
		    &action->focus_zoom_pane) != 0 ||
		    masil_action_parse_u64(yyjson_obj_get(item, "pty_generation"),
		    &action->focus_zoom_generation) != 0)
			return (-1);
		action->focus_restore_zoom = 1;
	}
	action->focus_args = 1;
	return (0);
}

static int
masil_action_parse_action(yyjson_val *value, struct masil_action_data *action)
{
	static const char *const keys_fields[] = { "kind", "type", "keys" };
	static const char *const no_argument_fields[] = { "kind", "type" };
	static const char *const input_fields[] = {
		"kind", "type", "staging_id", "bracketed", "submit"
	};
	yyjson_val *kind, *type, *item;
	const char *string;
	size_t length;

	if (yyjson_is_str(value)) {
		string = yyjson_get_str(value);
		length = yyjson_get_len(value);
		action->kind = masil_action_kind_from_string(string, length);
		/* A bare "input_commit" names no staging: unsupported, as in C1a. */
		if (action->kind == MASIL_ACTION_INPUT_COMMIT)
			action->input_legacy = 1;
		return (action->kind == MASIL_ACTION_KEYS ? -1 : 0);
	}
	if (!yyjson_is_obj(value))
		return (-1);
	kind = yyjson_obj_get(value, "kind");
	type = yyjson_obj_get(value, "type");
	if (kind != NULL && !yyjson_is_str(kind))
		return (-1);
	if (type != NULL && !yyjson_is_str(type))
		return (-1);
	if (kind != NULL && type != NULL &&
	    (yyjson_get_len(kind) != yyjson_get_len(type) ||
	    memcmp(yyjson_get_str(kind), yyjson_get_str(type),
	    yyjson_get_len(kind)) != 0))
		return (-1);
	if (kind == NULL)
		kind = type;
	if (kind == NULL) {
		if (yyjson_obj_get(value, "keys") == NULL)
			return (-1);
		action->kind = MASIL_ACTION_KEYS;
	} else {
		string = yyjson_get_str(kind);
		length = yyjson_get_len(kind);
		action->kind = masil_action_kind_from_string(string, length);
	}
	if (action->kind == MASIL_ACTION_KEYS) {
		if (masil_action_schema(value, keys_fields, nitems(keys_fields)) != 0)
			return (-1);
		action->keys = yyjson_obj_get(value, "keys");
		if (!yyjson_is_arr(action->keys) ||
		    yyjson_arr_size(action->keys) > MASIL_ACTION_MAX_KEYS)
			return (-1);
	} else if (action->kind == MASIL_ACTION_KILL_PANE) {
		if (masil_action_schema(value, no_argument_fields,
		    nitems(no_argument_fields)) != 0)
			return (-1);
	} else if (action->kind == MASIL_ACTION_INPUT_COMMIT) {
		/* Keep C1a's argument-free placeholder safely non-effectful. */
		if (masil_action_schema(value, no_argument_fields,
		    nitems(no_argument_fields)) == 0) {
			action->input_legacy = 1;
			return (0);
		}
		if (masil_action_schema(value, input_fields,
		    nitems(input_fields)) != 0 ||
		    masil_action_parse_string(value, "staging_id", &action->staging_id,
		    &action->staging_id_length) != 0 ||
		    !masil_action_staging_id_valid(action->staging_id,
		    action->staging_id_length))
			return (-1);
		item = yyjson_obj_get(value, "bracketed");
		if (!yyjson_is_bool(item))
			return (-1);
		action->bracketed = yyjson_get_bool(item);
		item = yyjson_obj_get(value, "submit");
		if (!yyjson_is_str(item))
			return (-1);
		string = yyjson_get_str(item);
		length = yyjson_get_len(item);
		if (length == 4 && memcmp(string, "none", length) == 0)
			action->submit_enter = 0;
		else if (length == 5 && memcmp(string, "enter", length) == 0)
			action->submit_enter = 1;
		else
			return (-1);
	} else if (action->kind == MASIL_ACTION_LAUNCH) {
		/* Keep C1a's no-argument placeholder non-effectful. */
		if (masil_action_schema(value, no_argument_fields,
		    nitems(no_argument_fields)) == 0)
			return (0);
		if (masil_action_parse_launch(action, value) != 0)
			return (-1);
	} else if (action->kind == MASIL_ACTION_FOCUS) {
		/* Keep the no-argument placeholder non-effectful. */
		if (masil_action_schema(value, no_argument_fields,
		    nitems(no_argument_fields)) == 0)
			return (0);
		if (masil_action_parse_focus(action, value) != 0)
			return (-1);
	}
	return (0);
}

static int
masil_action_input_preconditions_valid(const struct masil_action_request *request)
{
	const struct masil_action_preconditions *preconditions =
	    &request->preconditions;

	return (preconditions->has_mode && preconditions->mode_none &&
	    preconditions->has_output_generation &&
	    preconditions->has_tracked_digest && preconditions->has_title &&
	    preconditions->has_progress);
}

static int
masil_action_key_equal(const struct masil_action_key *left,
    const struct masil_action_key *right)
{
	return (left->length == right->length &&
	    memcmp(left->bytes, right->bytes, left->length) == 0);
}

static void
masil_action_staging_release(struct masil_action_staging_slot *slot)
{
	char *data = slot->data;

	if (data != NULL)
		memset(data, 0, MASIL_ACTION_STAGING_BYTES);
	memset(slot, 0, sizeof *slot);
	slot->data = data;
}

static void
masil_action_staging_expire(void)
{
	uint64_t now = get_timer();
	size_t i;

	for (i = 0; i < nitems(masil_action_staging); i++) {
		if (masil_action_staging[i].active &&
		    masil_action_staging[i].deadline <= now)
			masil_action_staging_release(&masil_action_staging[i]);
	}
}

static void masil_action_staging_schedule(void);

static void
masil_action_staging_timeout(__unused int fd, __unused short events,
    __unused void *data)
{
	masil_action_staging_timer_active = 0;
	masil_action_staging_expire();
	masil_action_staging_schedule();
}

static void
masil_action_staging_schedule(void)
{
	struct timeval tv;
	uint64_t earliest = 0, now, remaining;
	size_t i;

	if (masil_action_staging_timer_active) {
		evtimer_del(&masil_action_staging_event);
		masil_action_staging_timer_active = 0;
	}
	for (i = 0; i < nitems(masil_action_staging); i++) {
		if (!masil_action_staging[i].active)
			continue;
		if (earliest == 0 || masil_action_staging[i].deadline < earliest)
			earliest = masil_action_staging[i].deadline;
	}
	if (earliest == 0)
		return;
	now = get_timer();
	remaining = earliest > now ? earliest - now : 0;
	tv.tv_sec = remaining / 1000;
	tv.tv_usec = (remaining % 1000) * 1000;
	evtimer_set(&masil_action_staging_event, masil_action_staging_timeout,
	    NULL);
	evtimer_add(&masil_action_staging_event, &tv);
	masil_action_staging_timer_active = 1;
}

static struct masil_action_staging_slot *
masil_action_staging_find(const char *id, size_t length)
{
	size_t i;

	for (i = 0; i < nitems(masil_action_staging); i++) {
		if (masil_action_staging[i].active &&
		    strlen(masil_action_staging[i].id) == length &&
		    memcmp(masil_action_staging[i].id, id, length) == 0)
			return (&masil_action_staging[i]);
	}
	return (NULL);
}

static struct masil_action_staging_slot *
masil_action_staging_allocate(void)
{
	size_t i;

	for (i = 0; i < nitems(masil_action_staging); i++) {
		if (masil_action_staging[i].active)
			continue;
		if (masil_action_staging[i].data == NULL)
			masil_action_staging[i].data = xmalloc(MASIL_ACTION_STAGING_BYTES);
		return (&masil_action_staging[i]);
	}
	return (NULL);
}

static void
masil_action_staging_close(void)
{
	size_t i;

	if (masil_action_staging_timer_active) {
		evtimer_del(&masil_action_staging_event);
		masil_action_staging_timer_active = 0;
	}
	for (i = 0; i < nitems(masil_action_staging); i++) {
		free(masil_action_staging[i].data);
		memset(&masil_action_staging[i], 0,
		    sizeof masil_action_staging[i]);
	}
}

static enum masil_action_reason
masil_action_staging_check(struct masil_action_staging_slot *slot,
    const struct masil_action_request *request)
{
	struct masil_action_sha256_ctx sha256;
	unsigned char digest[32];

	if (slot->received != slot->total_len)
		return (MASIL_ACTION_REASON_STAGING_INCOMPLETE);
	sha256 = slot->sha256;
	masil_action_sha256_final(&sha256, digest);
	if (memcmp(digest, slot->expected_digest, sizeof digest) != 0)
		return (MASIL_ACTION_REASON_STAGING_DIGEST_MISMATCH);
	if (!masil_action_key_equal(&slot->operation_key,
	    &request->operation_key))
		return (MASIL_ACTION_REASON_STAGING_KEY_MISMATCH);
	return (MASIL_ACTION_REASON_NONE);
}

/* masil: epochs are boot-local; retained closures are never evicted for age. */
int
masil_action_supported(void)
{
	return (1);
}

static size_t
masil_action_closed_empty_count(void)
{
	size_t i, count = 0;

	for (i = 0; i < masil_action_closed_count; i++) {
		if (masil_action_closed_epochs[i].slots_used == 0)
			count++;
	}
	return (count);
}

static void
masil_action_closed_epoch_evict_oldest_empty(void)
{
	size_t i;

	for (i = 0; i < masil_action_closed_count; i++) {
		if (masil_action_closed_epochs[i].slots_used == 0)
			break;
	}
	if (i == masil_action_closed_count)
		fatalx("%s: closed epoch table has no empty entry", __func__);
	if (i + 1 < masil_action_closed_count) {
		memmove(&masil_action_closed_epochs[i],
		    &masil_action_closed_epochs[i + 1],
		    (masil_action_closed_count - i - 1) *
		    sizeof *masil_action_closed_epochs);
	}
	masil_action_closed_count--;
}

static void
masil_action_closed_epoch_trim_empty(size_t maximum)
{
	/* masil: slot-free epochs retain only the newest watermark history. */
	while (masil_action_closed_empty_count() > maximum)
		masil_action_closed_epoch_evict_oldest_empty();
}

uint64_t
masil_action_coordinator_open(void)
{
	if (masil_action_active_epoch != 0)
		fatalx("%s: coordinator epoch already active", __func__);
	if (masil_action_next_epoch == UINT64_MAX)
		return (0);
	if (masil_action_slots == NULL) {
		masil_action_slots = xcalloc(MASIL_ACTION_SLOTS,
		    sizeof *masil_action_slots);
		masil_action_closed_epochs = xcalloc(
		    MASIL_ACTION_CLOSED_EPOCH_CAPACITY,
		    sizeof *masil_action_closed_epochs);
	}
	/*
	 * Each retained closed epoch has a table entry, while eight empty entries
	 * preserve recent watermarks. Only the epoch counter can exhaust hello.
	 */
	masil_action_active_epoch = ++masil_action_next_epoch;
	masil_action_active_next_seq = 0;
	masil_action_active_slots_used = 0;
	return (masil_action_active_epoch);
}

void
masil_action_coordinator_close(uint64_t epoch)
{
	size_t index;

	if (epoch == 0 || epoch != masil_action_active_epoch)
		return;
	/* masil: C2a input buffers belong to the coordinator connection. */
	masil_action_staging_close();
	masil_action_closed_epoch_trim_empty(masil_action_active_slots_used == 0 ?
	    MASIL_ACTION_CLOSED_EPOCHS - 1 : MASIL_ACTION_CLOSED_EPOCHS);
	/* The 2,048-slot bound guarantees a zero-slot entry when this is full. */
	if (masil_action_closed_count == MASIL_ACTION_CLOSED_EPOCH_CAPACITY)
		masil_action_closed_epoch_evict_oldest_empty();
	index = masil_action_closed_count++;
	masil_action_closed_epochs[index].epoch = epoch;
	masil_action_closed_epochs[index].watermark = masil_action_active_next_seq;
	masil_action_closed_epochs[index].slots_used =
	    masil_action_active_slots_used;
	masil_action_active_epoch = 0;
	masil_action_active_next_seq = 0;
	masil_action_active_slots_used = 0;
}

static struct masil_action_closed_epoch *
masil_action_closed_epoch_find(uint64_t epoch)
{
	size_t i;

	for (i = 0; i < masil_action_closed_count; i++) {
		if (masil_action_closed_epochs[i].epoch == epoch)
			return (&masil_action_closed_epochs[i]);
	}
	return (NULL);
}

static int
masil_action_closed_watermark(uint64_t epoch, uint64_t *watermark)
{
	struct masil_action_closed_epoch *closed;

	closed = masil_action_closed_epoch_find(epoch);
	if (closed == NULL)
		return (0);
	*watermark = closed->watermark;
	return (1);
}

static int
masil_action_known_epoch(uint64_t epoch, uint64_t *watermark)
{
	if (epoch != 0 && epoch == masil_action_active_epoch) {
		*watermark = masil_action_active_next_seq;
		return (1);
	}
	return (masil_action_closed_watermark(epoch, watermark));
}

static struct masil_action_slot *
masil_action_slot_find(const struct masil_action_ticket *ticket)
{
	size_t i;

	if (masil_action_slots == NULL)
		return (NULL);
	for (i = 0; i < MASIL_ACTION_SLOTS; i++) {
		if (masil_action_slots[i].used &&
		    masil_action_slots[i].epoch == ticket->epoch &&
		    masil_action_slots[i].seq == ticket->seq)
			return (&masil_action_slots[i]);
	}
	return (NULL);
}

static struct masil_action_slot *
masil_action_slot_allocate(void)
{
	struct masil_action_slot *slot = NULL;
	size_t i;

	if (masil_action_slots == NULL ||
	    masil_action_slots_used == MASIL_ACTION_SLOTS)
		return (NULL);
	for (i = 0; i < MASIL_ACTION_SLOTS; i++) {
		slot = &masil_action_slots[masil_action_slot_cursor];
		masil_action_slot_cursor = (masil_action_slot_cursor + 1) %
		    MASIL_ACTION_SLOTS;
		if (!slot->used)
			return (slot);
	}
	return (NULL);
}

static void
masil_action_slot_store(struct masil_action_slot *slot,
    const struct masil_action_request *request)
{
	memset(slot, 0, sizeof *slot);
	slot->epoch = request->ticket.epoch;
	slot->seq = request->ticket.seq;
	memcpy(slot->payload_digest, request->payload_digest,
	    sizeof slot->payload_digest);
	slot->key_length = request->operation_key.length;
	memcpy(slot->key, request->operation_key.bytes, slot->key_length);
	slot->used = 1;
	masil_action_slots_used++;
	masil_action_active_slots_used++;
}

static void
masil_action_slot_release(struct masil_action_slot *slot)
{
	struct masil_action_closed_epoch *closed;
	uint64_t epoch;

	if (!slot->used)
		return;
	epoch = slot->epoch;
	memset(slot, 0, sizeof *slot);
	masil_action_slots_used--;
	if (epoch == masil_action_active_epoch) {
		masil_action_active_slots_used--;
		return;
	}
	closed = masil_action_closed_epoch_find(epoch);
	/*
	 * An emptied epoch keeps its watermark until the next coordinator close,
	 * so a coordinator reconciling it still gets not_applied answers.
	 */
	if (closed != NULL)
		closed->slots_used--;
}

static int
masil_action_slot_matches(const struct masil_action_slot *slot,
    const struct masil_action_key *operation_key,
    const unsigned char *payload_digest)
{
	return (slot->key_length == operation_key->length &&
	    memcmp(slot->key, operation_key->bytes, slot->key_length) == 0 &&
	    memcmp(slot->payload_digest, payload_digest,
	    sizeof slot->payload_digest) == 0);
}

static int
masil_action_slot_component(const struct masil_action_slot *slot, size_t wanted,
    const unsigned char **value, size_t *length)
{
	size_t i, offset = 0, item_length;

	for (i = 0; i < 5; i++) {
		if (offset >= slot->key_length)
			return (-1);
		item_length = slot->key[offset++];
		if (item_length > slot->key_length - offset)
			return (-1);
		if (i == wanted) {
			*value = slot->key + offset;
			*length = item_length;
			return (0);
		}
		offset += item_length;
	}
	return (-1);
}

static int
masil_action_string_equal(const char *actual, const char *expected,
    size_t expected_length)
{
	return (strlen(actual) == expected_length &&
	    memcmp(actual, expected, expected_length) == 0);
}

static int
masil_action_core_boot_matches(const char *boot_id, size_t boot_id_length)
{
	return (masil_action_string_equal(masil_bridge_get_boot_id(), boot_id,
	    boot_id_length));
}

static const char *
masil_action_option_value(struct window_pane *wp, const char *name)
{
	struct options_entry *entry;

	entry = options_get(wp->options, name);
	if (entry == NULL || !options_is_string(entry))
		return ("");
	return (options_get_string(wp->options, name));
}

static char *
masil_action_current_command(struct window_pane *wp)
{
	char *command, *value;

	if (wp->shell == NULL)
		return (xstrdup(""));
	command = osdep_get_name(wp->fd, wp->tty);
	if (command == NULL || *command == '\0') {
		free(command);
		command = cmd_stringify_argv(wp->argc, wp->argv);
		if (command == NULL || *command == '\0') {
			free(command);
			command = xstrdup(wp->shell);
		}
	}
	value = parse_window_name(command);
	free(command);
	return (value);
}

static int
masil_action_parse_id(const char *value, size_t length, char prefix,
    u_int *idp)
{
	uint64_t id = 0, digit;
	size_t i;

	if (length < 2 || value[0] != prefix)
		return (-1);
	for (i = 1; i < length; i++) {
		if (value[i] < '0' || value[i] > '9')
			return (-1);
		digit = value[i] - '0';
		if (id > (UINT_MAX - digit) / 10)
			return (-1);
		id = id * 10 + digit;
	}
	*idp = id;
	return (0);
}

static int
masil_action_parse_pane_id(const char *value, size_t length, u_int *pane_id)
{
	return (masil_action_parse_id(value, length, '%', pane_id));
}

static int
masil_action_parse_session_id(const char *value, size_t length, u_int *session_id)
{
	return (masil_action_parse_id(value, length, '$', session_id));
}

static enum masil_action_reason
masil_action_check_launch_preconditions(const struct masil_action_request *request)
{
	struct window_pane	*wp;
	u_int			 pane_id, session_id, target_pane_id;

	if (!masil_action_string_equal(masil_bridge_get_boot_id(),
	    request->target.core_boot_id, request->target.core_boot_id_length))
		return (MASIL_ACTION_REASON_CORE_BOOT_CHANGED);
	if (request->action.launch_mode == MASIL_ACTION_LAUNCH_WINDOW) {
		if (masil_action_parse_session_id(request->action.launch_target,
		    request->action.launch_target_length, &session_id) != 0 ||
		    session_find_by_id(session_id) == NULL)
			return (MASIL_ACTION_REASON_TARGET_GONE);
		return (MASIL_ACTION_REASON_NONE);
	}
	if (masil_action_parse_pane_id(request->action.launch_target,
	    request->action.launch_target_length, &pane_id) != 0 ||
	    masil_action_parse_pane_id(request->target.pane_id,
	    request->target.pane_id_length, &target_pane_id) != 0 ||
	    pane_id != target_pane_id)
		return (MASIL_ACTION_REASON_TARGET_GONE);
	wp = window_pane_find_by_id(pane_id);
	if (wp == NULL)
		return (MASIL_ACTION_REASON_TARGET_GONE);
	if (wp->masil_generation_exhausted ||
	    wp->masil_pty_generation != request->target.pty_generation)
		return (MASIL_ACTION_REASON_PTY_GENERATION_CHANGED);
	return (MASIL_ACTION_REASON_NONE);
}

/* masil: a focus target needs only boot, identity, generation and liveness. */
static enum masil_action_reason
masil_action_check_focus_preconditions(const struct masil_action_request *request,
    struct window_pane **wpp)
{
	struct window_pane	*wp;
	u_int			 pane_id;

	if (!masil_action_string_equal(masil_bridge_get_boot_id(),
	    request->target.core_boot_id, request->target.core_boot_id_length))
		return (MASIL_ACTION_REASON_CORE_BOOT_CHANGED);
	if (masil_action_parse_pane_id(request->target.pane_id,
	    request->target.pane_id_length, &pane_id) != 0)
		return (MASIL_ACTION_REASON_TARGET_GONE);
	wp = window_pane_find_by_id(pane_id);
	if (wp == NULL)
		return (MASIL_ACTION_REASON_TARGET_GONE);
	if (request->preconditions.pane_dead || wp->fd == -1)
		return (MASIL_ACTION_REASON_PANE_DEAD);
	if (wp->masil_generation_exhausted ||
	    wp->masil_pty_generation != request->target.pty_generation)
		return (MASIL_ACTION_REASON_PTY_GENERATION_CHANGED);
	*wpp = wp;
	return (MASIL_ACTION_REASON_NONE);
}

static enum masil_action_reason
masil_action_check_preconditions(const struct masil_action_request *request,
    struct window_pane **wpp)
{
	struct window_pane *wp;
	unsigned char digest[32];
	const char *option, *title, *progress;
	char foreground[32], *command;
	pid_t pgid;
	u_int pane_id;

	if (!masil_action_string_equal(masil_bridge_get_boot_id(),
	    request->target.core_boot_id, request->target.core_boot_id_length))
		return (MASIL_ACTION_REASON_CORE_BOOT_CHANGED);
	if (masil_action_parse_pane_id(request->target.pane_id,
	    request->target.pane_id_length, &pane_id) != 0)
		return (MASIL_ACTION_REASON_TARGET_GONE);
	wp = window_pane_find_by_id(pane_id);
	if (wp == NULL)
		return (MASIL_ACTION_REASON_TARGET_GONE);
	if (request->preconditions.pane_dead || wp->fd == -1)
		return (MASIL_ACTION_REASON_PANE_DEAD);
	if (wp->masil_generation_exhausted ||
	    wp->masil_pty_generation != request->target.pty_generation)
		return (MASIL_ACTION_REASON_PTY_GENERATION_CHANGED);
	foreground[0] = '\0';
	if (wp->fd != -1 && (pgid = tcgetpgrp(wp->fd)) > 0)
		xsnprintf(foreground, sizeof foreground, "%ld", (long)pgid);
	if (!masil_action_string_equal(foreground,
	    request->preconditions.foreground_pgid,
	    request->preconditions.foreground_pgid_length))
		return (MASIL_ACTION_REASON_FOREGROUND_PGID_CHANGED);
	command = masil_action_current_command(wp);
	if (!masil_action_string_equal(command,
	    request->preconditions.current_command,
	    request->preconditions.current_command_length)) {
		free(command);
		return (MASIL_ACTION_REASON_CURRENT_COMMAND_CHANGED);
	}
	free(command);
	option = masil_action_option_value(wp, "@masil-managed-agent");
	masil_action_sha256(option, strlen(option), digest);
	if (memcmp(digest, request->preconditions.meta_digest, sizeof digest) != 0)
		return (MASIL_ACTION_REASON_META_CHANGED);
	if (request->preconditions.input_off || (wp->flags & PANE_INPUTOFF))
		return (MASIL_ACTION_REASON_INPUT_OFF);
	if (request->preconditions.synchronize ||
	    options_get_number(wp->options, "synchronize-panes"))
		return (MASIL_ACTION_REASON_SYNCHRONIZE_PANES);
	if (request->preconditions.has_mode && request->preconditions.mode_none &&
	    !TAILQ_EMPTY(&wp->modes))
		return (MASIL_ACTION_REASON_MODE_ACTIVE);
	if (request->preconditions.has_output_generation &&
	    wp->output_generation != request->preconditions.output_generation)
		return (MASIL_ACTION_REASON_OUTPUT_CHANGED);
	if (request->preconditions.has_tracked_digest) {
		option = masil_action_option_value(wp,
		    "@masil-managed-observation");
		masil_action_sha256(option, strlen(option), digest);
		if (memcmp(digest, request->preconditions.tracked_digest,
		    sizeof digest) != 0)
			return (MASIL_ACTION_REASON_TRACKED_CHANGED);
	}
	if (request->preconditions.has_title) {
		title = wp->base.title == NULL ? "" : wp->base.title;
		if (!masil_action_string_equal(title, request->preconditions.title,
		    request->preconditions.title_length))
			return (MASIL_ACTION_REASON_TITLE_CHANGED);
	}
	if (request->preconditions.has_progress) {
		progress = wp->base.masil_osc_progress;
		if (!masil_action_string_equal(progress,
		    request->preconditions.progress,
		    request->preconditions.progress_length))
			return (MASIL_ACTION_REASON_PROGRESS_CHANGED);
	}
	*wpp = wp;
	return (MASIL_ACTION_REASON_NONE);
}

static int
masil_action_key_writable(struct window_pane *wp, key_code key)
{
	key_code base = key & KEYC_MASK_KEY;

	if (KEYC_IS_MOUSE(key) || KEYC_IS_USER(key))
		return (0);
	if (KEYC_IS_PASTE(key) && (~wp->screen->mode & MODE_BRACKETPASTE))
		return (0);
	if (!KEYC_IS_SPECIAL(key))
		return (1);
	if (base == KEYC_PASTE_START || base == KEYC_PASTE_END ||
	    base == KEYC_BSPACE || (base >= KEYC_F1 && base <= KEYC_KP_PERIOD))
		return (1);
	/* Other special values (focus, reports, Any) never reach the pane PTY. */
	return (0);
}

/*
 * Keep this standard-mode dry run in step with input_key_vt10x in
 * input-keys.c. After input_key handles backspace and mapped special keys,
 * this is its Ctrl-key path that can return -1.
 */
static int
masil_action_standard_key_encodable(key_code key)
{
	static const char standard_map[] = "1!9(0)=+;:'\",<.>/-8? 2";
	key_code onlykey;

	if (KEYC_IS_UNICODE(key))
		return (1);
	if ((key & KEYC_MASK_KEY) == KEYC_BSPACE) {
		key = options_get_number(global_options, "backspace") |
		    (key & (KEYC_MASK_FLAGS|KEYC_MASK_MODIFIERS));
	}
	if ((key & KEYC_MASK_KEY) == KEYC_BTAB)
		key &= ~KEYC_MASK_MODIFIERS;
	if (KEYC_IS_SPECIAL(key))
		return (1);
	onlykey = key & KEYC_MASK_KEY;
	if (onlykey == '\r' || onlykey == '\n' || onlykey == '\t')
		key &= ~KEYC_CTRL;
	if (~key & KEYC_CTRL)
		return (1);
	return (strchr(standard_map, onlykey) != NULL ||
	    (onlykey >= '3' && onlykey <= '7') ||
	    (onlykey >= '@' && onlykey <= '~'));
}

static int
masil_action_key_needs_literal(struct window_pane *wp, key_code key)
{
	return ((wp->screen->mode & EXTENDED_KEY_MODES) == 0 &&
	    !masil_action_standard_key_encodable(key));
}

static enum masil_action_reason
masil_action_prevalidate_literal(struct window_pane *wp, const char *string,
    key_code output[MASIL_ACTION_MAX_KEYS], size_t *count)
{
	struct utf8_data *data, *loop;
	utf8_char character;
	key_code key;

	data = utf8_fromcstr(string);
	for (loop = data; loop->size != 0; loop++) {
		if (*count == MASIL_ACTION_MAX_KEYS) {
			free(data);
			return (MASIL_ACTION_REASON_INVALID_KEY);
		}
		if (loop->size == 1 && loop->data[0] <= 0x7f)
			key = loop->data[0];
		else {
			if (utf8_from_data(loop, &character) != UTF8_DONE) {
				free(data);
				return (MASIL_ACTION_REASON_INVALID_KEY);
			}
			key = character;
		}
		if (!masil_action_key_writable(wp, key)) {
			free(data);
			return (MASIL_ACTION_REASON_INVALID_KEY);
		}
		output[(*count)++] = key;
	}
	free(data);
	return (MASIL_ACTION_REASON_NONE);
}

static enum masil_action_reason
masil_action_prevalidate_keys(struct window_pane *wp, yyjson_val *values,
    key_code output[MASIL_ACTION_MAX_KEYS], size_t *output_count)
{
	yyjson_arr_iter iter;
	yyjson_val *value;
	key_code key;
	const char *string;
	size_t length, count = 0;

	iter = yyjson_arr_iter_with(values);
	while ((value = yyjson_arr_iter_next(&iter)) != NULL) {
		if (!yyjson_is_str(value))
			return (MASIL_ACTION_REASON_INVALID_KEY);
		string = yyjson_get_str(value);
		length = yyjson_get_len(value);
		if (length == 0 || length > MASIL_ACTION_MAX_KEYS * 4 ||
		    memchr(string, '\0', length) != NULL)
			return (MASIL_ACTION_REASON_INVALID_KEY);
		key = key_string_lookup_string(string);
		/* Keep cmd-send-keys parity: None and unknown names are literal text. */
		if (key != KEYC_NONE && key != KEYC_UNKNOWN) {
			if (!masil_action_key_writable(wp, key))
				return (MASIL_ACTION_REASON_INVALID_KEY);
			if (masil_action_key_needs_literal(wp, key)) {
				if (masil_action_prevalidate_literal(wp, string, output,
				    &count) != MASIL_ACTION_REASON_NONE)
					return (MASIL_ACTION_REASON_INVALID_KEY);
			} else {
				if (count == MASIL_ACTION_MAX_KEYS)
					return (MASIL_ACTION_REASON_INVALID_KEY);
				output[count++] = key;
			}
			continue;
		}
		if (masil_action_prevalidate_literal(wp, string, output,
		    &count) != MASIL_ACTION_REASON_NONE)
			return (MASIL_ACTION_REASON_INVALID_KEY);
	}
	*output_count = count;
	return (MASIL_ACTION_REASON_NONE);
}

static int
masil_action_pane_context(struct window_pane *wp, struct session **session,
    struct winlink **winlink)
{
	struct winlink *wl;

	TAILQ_FOREACH(wl, &wp->window->winlinks, wentry) {
		if (wl->session != NULL) {
			*session = wl->session;
			*winlink = wl;
			return (0);
		}
	}
	return (-1);
}

/* Keep C2a's direct buffer write byte-for-byte with paste-buffer -p -r. */
static void
masil_action_input_paste(struct window_pane *wp, const char *data, size_t size,
    int bracketed, uint32_t *queued_bytes)
{
	const char *end = data + size, *line;
	char *escaped;
	size_t length, queued = 0;

	if (bracketed) {
		bufferevent_write(wp->event, "\033[200~", 6);
		queued += 6;
	}
	for (;;) {
		line = memchr(data, '\n', end - data);
		if (line == NULL)
			break;
		length = utf8_stravisx(&escaped, data, line - data,
		    VIS_SAFE|VIS_NOSLASH);
		bufferevent_write(wp->event, escaped, length);
		free(escaped);
		bufferevent_write(wp->event, "\n", 1);
		queued += length + 1;
		data = line + 1;
	}
	if (data != end) {
		length = utf8_stravisx(&escaped, data, end - data,
		    VIS_SAFE|VIS_NOSLASH);
		bufferevent_write(wp->event, escaped, length);
		free(escaped);
		queued += length;
	}
	if (bracketed) {
		bufferevent_write(wp->event, "\033[201~", 6);
		queued += 6;
	}
	*queued_bytes = queued;
}

void
masil_launch_context_retain(struct masil_launch_context *context)
{
	context->references++;
}

void
masil_launch_context_release(struct masil_launch_context *context)
{
	if (--context->references != 0)
		return;
	if (context->status_fd != -1)
		close(context->status_fd);
	free(context->cwd);
	free(context->name);
	if (context->argv != NULL)
		cmd_free_argv(context->argc, context->argv);
	environ_free(context->environ);
	free(context->error);
	free(context);
}

static void
masil_launch_context_finish_status(struct masil_launch_context *context,
    const char *stage, int error)
{
	event_del(&context->status_event);
	close(context->status_fd);
	context->status_fd = -1;
	masil_bridge_launch(context->pane_id, context->pty_generation, stage,
	    error);
	masil_launch_context_release(context);
}

static void
masil_launch_context_status_callback(__unused int fd, short events, void *data)
{
	struct masil_launch_context *context = data;
	ssize_t			 used;
	char			 *at;
	const char		 *stage;
	int			 error;

	if ((events & EV_READ) == 0)
		return;
	for (;;) {
		at = (char *)&context->status + context->status_used;
		used = read(context->status_fd, at,
		    sizeof context->status - context->status_used);
		if (used > 0) {
			context->status_used += used;
			if (context->status_used != sizeof context->status)
				continue;
			switch (context->status.stage) {
			case MASIL_LAUNCH_STAGE_CWD_OPEN:
				stage = "cwd_open";
				break;
			case MASIL_LAUNCH_STAGE_CWD_IDENTITY:
				stage = "cwd_identity";
				break;
			case MASIL_LAUNCH_STAGE_EXEC:
				stage = "exec";
				break;
			default:
				stage = "exec";
				context->status.error = EPROTO;
				break;
			}
			error = context->status.error;
			masil_launch_context_finish_status(context, stage, error);
			return;
		}
		if (used == 0) {
			if (context->status_used == 0)
				masil_launch_context_finish_status(context, "exec_ok", 0);
			else
				masil_launch_context_finish_status(context, "exec", EIO);
			return;
		}
		if (errno == EINTR)
			continue;
		if (errno == EAGAIN || errno == EWOULDBLOCK)
			return;
		masil_launch_context_finish_status(context, "exec", errno);
		return;
	}
}

const char *
masil_launch_context_cwd(struct masil_launch_context *context)
{
	return (context->cwd);
}

const char *
masil_launch_context_name(struct masil_launch_context *context)
{
	return (context->name);
}

struct environ *
masil_launch_context_environ(struct masil_launch_context *context)
{
	struct environ *copy;

	copy = environ_create();
	environ_copy(context->environ, copy);
	return (copy);
}

void
masil_launch_context_argv(struct masil_launch_context *context, int *argc,
    char ***argv)
{
	*argc = context->argc;
	*argv = cmd_copy_argv(context->argc, context->argv);
}

void
masil_launch_context_command_started(struct masil_launch_context *context)
{
	context->command_started = 1;
}

int
masil_launch_context_cancelled(struct masil_launch_context *context)
{
	return (context->cancelled);
}

uint64_t
masil_launch_context_cwd_dev(struct masil_launch_context *context)
{
	return (context->cwd_dev);
}

uint64_t
masil_launch_context_cwd_ino(struct masil_launch_context *context)
{
	return (context->cwd_ino);
}

void
masil_launch_context_set_status_fd(struct masil_launch_context *context, int fd)
{
	if (context->status_fd != -1)
		close(context->status_fd);
	context->status_fd = fd;
}

void
masil_launch_context_pane_created(struct masil_launch_context *context,
    struct window_pane *wp)
{
	context->pane_id = wp->id;
	context->pid = wp->pid;
	context->pty_generation = wp->masil_pty_generation;
	context->pane_created = 1;
	if (context->status_fd == -1)
		return;
	setblocking(context->status_fd, 0);
	masil_launch_context_retain(context);
	event_set(&context->status_event, context->status_fd, EV_READ|EV_PERSIST,
	    masil_launch_context_status_callback, context);
	event_add(&context->status_event, NULL);
}

void
masil_launch_context_set_error(struct masil_launch_context *context,
    const char *fmt, ...)
{
	va_list ap;

	free(context->error);
	va_start(ap, fmt);
	xvasprintf(&context->error, fmt, ap);
	va_end(ap);
}

static struct masil_launch_context *
masil_launch_context_create(const struct masil_action_data *action,
    yyjson_val *env, yyjson_val *argv)
{
	struct masil_launch_context *context;
	yyjson_arr_iter		 iter;
	yyjson_val		*item, *key, *value;
	const char		*string;
	char			*entry;
	size_t			 length;
	int			 i = 0;

	context = xcalloc(1, sizeof *context);
	context->references = 1;
	context->cwd = xstrndup(action->launch_cwd,
	    action->launch_cwd_length);
	if (action->launch_has_name)
		context->name = xstrndup(action->launch_name,
		    action->launch_name_length);
	context->environ = environ_create();
	iter = yyjson_arr_iter_with(env);
	while ((item = yyjson_arr_iter_next(&iter)) != NULL) {
		key = yyjson_arr_get(item, 0);
		value = yyjson_arr_get(item, 1);
		xasprintf(&entry, "%.*s=%.*s", (int)yyjson_get_len(key),
		    yyjson_get_str(key), (int)yyjson_get_len(value),
		    yyjson_get_str(value));
		environ_put(context->environ, entry, 0);
		free(entry);
	}
	context->argc = (int)yyjson_arr_size(argv);
	context->argv = xcalloc(context->argc + 1, sizeof *context->argv);
	iter = yyjson_arr_iter_with(argv);
	while ((item = yyjson_arr_iter_next(&iter)) != NULL) {
		string = yyjson_get_str(item);
		length = yyjson_get_len(item);
		context->argv[i++] = xstrndup(string, length);
	}
	context->cwd_dev = action->launch_cwd_dev;
	context->cwd_ino = action->launch_cwd_ino;
	context->status_fd = -1;
	return (context);
}

static enum cmd_retval
masil_action_launch_complete(__unused struct cmdq_item *item, void *data)
{
	struct masil_launch_context *context = data;

	masil_launch_context_release(context);
	return (CMD_RETURN_NORMAL);
}

static yyjson_doc *
masil_action_launch_staged_spec(struct masil_action_staging_slot *staging,
    yyjson_val **envp, yyjson_val **argvp)
{
	static const char *const fields[] = { "env", "argv" };
	yyjson_doc	*document;
	yyjson_val	*root, *env, *argv;

	document = yyjson_read(staging->data, staging->total_len,
	    YYJSON_READ_NOFLAG);
	if (document == NULL)
		return (NULL);
	root = yyjson_doc_get_root(document);
	if (masil_action_schema(root, fields, nitems(fields)) != 0 ||
	    masil_action_parse_launch_values((env = yyjson_obj_get(root, "env")),
	    (argv = yyjson_obj_get(root, "argv"))) != 0) {
		yyjson_doc_free(document);
		return (NULL);
	}
	*envp = env;
	*argvp = argv;
	return (document);
}

static enum masil_action_result
masil_action_apply_launch(const struct masil_action_request *request,
    struct masil_action_staging_slot *staging, enum masil_action_reason *reason,
    struct masil_action_outcome *outcome)
{
	struct masil_launch_context *context;
	struct cmd_parse_input	 input = { .flags = CMD_PARSE_NOALIAS };
	struct cmd_parse_result	 *parse;
	struct cmdq_state	 *state;
	struct args_value	 *values;
	struct cmd		 *cmd;
	yyjson_doc		 *document = NULL;
	yyjson_val		 *env, *argv;
	char			 **command;
	size_t			 position = 0;
	u_int			 target_id;
	int			 argc;

	if (request->action.launch_staged) {
		document = masil_action_launch_staged_spec(staging, &env, &argv);
		if (document == NULL) {
			*reason = MASIL_ACTION_REASON_LAUNCH_FAILED;
			return (MASIL_ACTION_RESULT_REJECTED);
		}
	} else {
		env = request->action.launch_env;
		argv = request->action.launch_argv;
	}
	if (request->action.launch_mode == MASIL_ACTION_LAUNCH_WINDOW) {
		if (masil_action_parse_session_id(request->action.launch_target,
		    request->action.launch_target_length, &target_id) != 0) {
			if (document != NULL)
				yyjson_doc_free(document);
			*reason = MASIL_ACTION_REASON_LAUNCH_FAILED;
			return (MASIL_ACTION_RESULT_REJECTED);
		}
	} else if (masil_action_parse_pane_id(request->action.launch_target,
	    request->action.launch_target_length, &target_id) != 0) {
		if (document != NULL)
			yyjson_doc_free(document);
		*reason = MASIL_ACTION_REASON_LAUNCH_FAILED;
		return (MASIL_ACTION_RESULT_REJECTED);
	}
	context = masil_launch_context_create(&request->action, env, argv);
	if (document != NULL)
		yyjson_doc_free(document);
	command = xcalloc(5, sizeof *command);
	if (request->action.launch_mode == MASIL_ACTION_LAUNCH_WINDOW) {
		command[position++] = xstrdup("new-window");
		command[position++] = xstrdup("-d");
		command[position++] = xstrdup("-t");
		xasprintf(&command[position++], "$%u", target_id);
	} else {
		command[position++] = xstrdup("split-window");
		command[position++] = xstrdup("-h");
		command[position++] = xstrdup("-d");
		command[position++] = xstrdup("-t");
		xasprintf(&command[position++], "%%%u", target_id);
	}
	argc = position;
	values = args_from_vector(argc, command);
	parse = cmd_parse_from_arguments(values, argc, &input);
	args_free_values(values, argc);
	free(values);
	cmd_free_argv(argc, command);
	if (parse->status != CMD_PARSE_SUCCESS) {
		if (parse->error != NULL)
			masil_launch_context_set_error(context, "%s", parse->error);
		else
			masil_launch_context_set_error(context, "launch command parse failed");
		free(parse->error);
		masil_launch_context_release(context);
		*reason = MASIL_ACTION_REASON_LAUNCH_FAILED;
		return (MASIL_ACTION_RESULT_REJECTED);
	}
	cmd = cmd_list_first(parse->cmdlist);
	if (cmd == NULL || cmd_list_next(cmd) != NULL) {
		/* masil: fixed launch parsing must never admit a command sequence. */
		masil_launch_context_set_error(context,
		    "launch command parse produced multiple commands");
		cmd_list_free(parse->cmdlist);
		masil_launch_context_release(context);
		*reason = MASIL_ACTION_REASON_LAUNCH_FAILED;
		return (MASIL_ACTION_RESULT_REJECTED);
	}
	state = cmdq_new_state(NULL, NULL, 0);
	cmdq_set_masil_launch(state, context);
	cmdq_append(NULL, cmdq_get_command(parse->cmdlist, state));
	cmd_list_free(parse->cmdlist);
	cmdq_free_state(state);
	/* The cmdq state and this completion callback both retain context. */
	masil_launch_context_retain(context);
	cmdq_append(NULL, cmdq_get_callback(masil_action_launch_complete, context));
	(void)cmdq_next(NULL);
	if (!context->command_started)
		/* masil: a blocked global queue must not create this pane later. */
		context->cancelled = 1;
	if (!context->pane_created) {
		if (context->error == NULL)
			masil_launch_context_set_error(context, "launch command failed");
		masil_launch_context_release(context);
		*reason = MASIL_ACTION_REASON_LAUNCH_FAILED;
		return (MASIL_ACTION_RESULT_REJECTED);
	}
	outcome->launch = 1;
	outcome->pane_id = context->pane_id;
	outcome->pid = context->pid;
	outcome->pty_generation = context->pty_generation;
	masil_launch_context_release(context);
	return (MASIL_ACTION_RESULT_APPLIED);
}

/* masil: only a live attached-or-attaching client with a session can focus. */
static int
masil_action_client_live(struct client *c)
{
	return ((c->flags & CLIENT_DEAD) == 0 && c->session != NULL &&
	    c->session->curw != NULL);
}

/* The id is the bridge client id, "<core boot id>:<client serial>". */
static struct client *
masil_action_find_client(const char *id, size_t length)
{
	const char	*boot = masil_bridge_get_boot_id();
	size_t		 boot_length = strlen(boot), i;
	uint64_t	 serial = 0, digit;
	struct client	*c;

	if (boot_length == 0 || length <= boot_length + 1 ||
	    memcmp(id, boot, boot_length) != 0 || id[boot_length] != ':')
		return (NULL);
	for (i = boot_length + 1; i < length; i++) {
		if (id[i] < '0' || id[i] > '9')
			return (NULL);
		digit = id[i] - '0';
		if (serial > (UINT64_MAX - digit) / 10)
			return (NULL);
		serial = serial * 10 + digit;
	}
	TAILQ_FOREACH(c, &clients, entry) {
		if (c->masil_serial == serial)
			return (masil_action_client_live(c) ? c : NULL);
	}
	return (NULL);
}

/*
 * masil: D1a focus equals switch-client -E -c <client> -t <pane>. Every check
 * and the affected-client list come before the first change.
 */
static enum masil_action_result
masil_action_apply_focus(const struct masil_action_request *request,
    struct window_pane *wp, enum masil_action_reason *reason,
    struct masil_action_outcome *outcome)
{
	const struct masil_action_data	*action = &request->action;
	struct client			*tc, *other;
	struct cmd_find_state		 fs;
	struct session			*s;
	struct winlink			*wl;
	struct window			*w, *zoomed = NULL;
	int				 affected;

	tc = masil_action_find_client(action->focus_client_id,
	    action->focus_client_id_length);
	if (tc == NULL) {
		*reason = MASIL_ACTION_REASON_CLIENT_ABSENT;
		return (MASIL_ACTION_RESULT_REJECTED);
	}
	if (tc->flags & CLIENT_READONLY) {
		*reason = MASIL_ACTION_REASON_CLIENT_READONLY;
		return (MASIL_ACTION_RESULT_REJECTED);
	}
	if (tc->masil_view_revision != action->focus_view_revision) {
		*reason = MASIL_ACTION_REASON_VIEW_CHANGED;
		return (MASIL_ACTION_RESULT_REJECTED);
	}
	if (action->focus_restore_zoom) {
		zoomed = window_find_by_id(action->focus_zoom_window);
		if (zoomed == NULL || (~zoomed->flags & WINDOW_ZOOMED) ||
		    zoomed->active == NULL ||
		    zoomed->active->id != action->focus_zoom_pane ||
		    zoomed->active->masil_pty_generation !=
		    action->focus_zoom_generation) {
			*reason = MASIL_ACTION_REASON_ZOOM_CHANGED;
			return (MASIL_ACTION_RESULT_REJECTED);
		}
	}
	/* The session choice is the one switch-client -t %N would make. */
	if (cmd_find_from_pane(&fs, wp, 0) != 0) {
		*reason = MASIL_ACTION_REASON_WINDOW_UNLINKED;
		return (MASIL_ACTION_RESULT_REJECTED);
	}
	s = fs.s;
	wl = fs.wl;
	w = wl->window;

	TAILQ_FOREACH(other, &clients, entry) {
		if (other == tc || !masil_action_client_live(other))
			continue;
		affected = 0;
		if (other->session == s && s->curw != wl)
			affected = 1;
		if (wp != w->active && other->session->curw->window == w)
			affected = 1;
		/* Unzooming changes the view of every client on that window. */
		if (zoomed != NULL && other->session->curw->window == zoomed)
			affected = 1;
		if (!affected)
			continue;
		if (outcome->focus_affected_count < MASIL_ACTION_FOCUS_AFFECTED) {
			outcome->focus_affected[outcome->focus_affected_count].serial =
			    other->masil_serial;
			outcome->focus_affected[outcome->focus_affected_count].stalled =
			    masil_client_stalled(other);
		}
		outcome->focus_affected_count++;
	}
	outcome->focus_affected_listed = 1;
	if (!action->focus_shared && outcome->focus_affected_count != 0) {
		*reason = MASIL_ACTION_REASON_SHARED_FOCUS_CONFLICT;
		return (MASIL_ACTION_RESULT_REJECTED);
	}

	if (zoomed != NULL) {
		/* As resize-pane -Z unzooms. */
		window_unzoom(zoomed, 1);
		server_redraw_window(zoomed);
	}
	cmd_switch_client_select(s, wl, wp, 0, NULL);
	server_client_set_session(tc, s);
	server_client_set_key_table(tc, NULL);

	outcome->focus_applied = 1;
	outcome->focus_view_revision = tc->masil_view_revision;
	masil_bridge_focus_begin(tc, request->ticket.epoch, request->ticket.seq);
	return (MASIL_ACTION_RESULT_APPLIED);
}

static enum masil_action_result
masil_action_apply(const struct masil_action_request *request,
    struct window_pane *wp, struct masil_action_staging_slot *staging,
    enum masil_action_reason *reason,
    struct masil_action_outcome *outcome)
{
	struct session *session;
	struct winlink *winlink;
	key_code keys[MASIL_ACTION_MAX_KEYS];
	size_t count, i;

	*reason = MASIL_ACTION_REASON_NONE;
	memset(outcome, 0, sizeof *outcome);
	switch (request->action.kind) {
	case MASIL_ACTION_KEYS:
		if (!TAILQ_EMPTY(&wp->modes)) {
			*reason = MASIL_ACTION_REASON_MODE_ACTIVE;
			return (MASIL_ACTION_RESULT_REJECTED);
		}
		if (masil_action_pane_context(wp, &session, &winlink) != 0) {
			*reason = MASIL_ACTION_REASON_WINDOW_UNLINKED;
			return (MASIL_ACTION_RESULT_REJECTED);
		}
		*reason = masil_action_prevalidate_keys(wp, request->action.keys,
		    keys, &count);
		if (*reason != MASIL_ACTION_REASON_NONE)
			return (MASIL_ACTION_RESULT_REJECTED);
		/* All standard-key literal fallbacks were resolved before this write. */
		for (i = 0; i < count; i++)
			(void)window_pane_key(wp, NULL, session, winlink, keys[i], NULL);
		return (MASIL_ACTION_RESULT_APPLIED);
	case MASIL_ACTION_KILL_PANE:
		if (masil_action_pane_context(wp, &session, &winlink) != 0) {
			*reason = MASIL_ACTION_REASON_WINDOW_UNLINKED;
			return (MASIL_ACTION_RESULT_REJECTED);
		}
		server_kill_pane(wp);
		return (MASIL_ACTION_RESULT_APPLIED);
	case MASIL_ACTION_INPUT_COMMIT:
		if (request->action.input_legacy)
			return (MASIL_ACTION_RESULT_UNSUPPORTED);
		if (request->action.bracketed &&
		    (~wp->screen->mode & MODE_BRACKETPASTE)) {
			*reason = MASIL_ACTION_REASON_BRACKET_MODE_OFF;
			return (MASIL_ACTION_RESULT_REJECTED);
		}
		if (request->action.submit_enter &&
		    masil_action_pane_context(wp, &session, &winlink) != 0) {
			*reason = MASIL_ACTION_REASON_WINDOW_UNLINKED;
			return (MASIL_ACTION_RESULT_REJECTED);
		}
		/*
		 * Like paste-buffer -p: brackets follow the pane's mode, and
		 * bracketed:true only requires that mode (checked above).
		 */
		masil_action_input_paste(wp, staging->data, staging->total_len,
		    (wp->screen->mode & MODE_BRACKETPASTE) != 0,
		    &outcome->queued_bytes);
		if (request->action.submit_enter &&
		    window_pane_key(wp, NULL, session, winlink, C0_CR, NULL) == 0)
			outcome->queued_bytes++;
		return (MASIL_ACTION_RESULT_APPLIED);
	case MASIL_ACTION_LAUNCH:
		if (request->action.launch_target == NULL)
			return (MASIL_ACTION_RESULT_UNSUPPORTED);
		return (masil_action_apply_launch(request, staging, reason, outcome));
	case MASIL_ACTION_FOCUS:
		if (!request->action.focus_args)
			return (MASIL_ACTION_RESULT_UNSUPPORTED);
		return (masil_action_apply_focus(request, wp, reason, outcome));
	case MASIL_ACTION_UNKNOWN:
		return (MASIL_ACTION_RESULT_UNSUPPORTED);
	}
	return (MASIL_ACTION_RESULT_UNSUPPORTED);
}

/* masil: a consumed ticket is never allowed to perform its effect twice. */
static int
masil_action_guarded_execute(uint64_t connection_epoch,
    const char *request_id, size_t request_id_length,
    struct masil_action_request *request, char *response, size_t response_size)
{
	struct masil_action_slot *slot = NULL;
	struct masil_action_staging_slot *staging = NULL;
	struct window_pane *wp = NULL;
	enum masil_action_result result;
	enum masil_action_reason reason;
	struct masil_action_outcome outcome = { 0 };
	uint64_t watermark;

	if (connection_epoch == 0 || connection_epoch != masil_action_active_epoch ||
	    request->ticket.epoch != connection_epoch) {
		if (masil_action_closed_watermark(request->ticket.epoch,
		    &watermark))
			return (masil_action_response_error(response, response_size,
			    request_id, request_id_length, "epoch_closed"));
		return (masil_action_response_error(response, response_size,
		    request_id, request_id_length, "epoch_unknown"));
	}
	if (request->ticket.seq < masil_action_active_next_seq) {
		slot = masil_action_slot_find(&request->ticket);
		if (slot == NULL)
			return (masil_action_response_error(response, response_size,
			    request_id, request_id_length, "receipt_not_retained"));
		if (!masil_action_slot_matches(slot, &request->operation_key,
		    request->payload_digest))
			return (masil_action_response_error(response, response_size,
			    request_id, request_id_length, "idempotency_conflict"));
		outcome.queued_bytes = slot->queued_bytes;
		outcome.launch = slot->launch;
		outcome.pane_id = slot->pane_id;
		outcome.pid = slot->pid;
		outcome.pty_generation = slot->pty_generation;
		return (masil_action_response_result(response, response_size,
		    "guarded_action", request_id, request_id_length, &request->ticket,
		    slot->result, slot->reason, &outcome));
	}
	if (request->ticket.seq > masil_action_active_next_seq)
		return (masil_action_response_error(response, response_size,
		    request_id, request_id_length, "ticket_gap"));
	if (masil_action_active_next_seq == UINT64_MAX)
		return (masil_action_response_error(response, response_size,
		    request_id, request_id_length, "sequence_exhausted"));
	if ((request->action.kind == MASIL_ACTION_INPUT_COMMIT &&
	    !request->action.input_legacy) ||
	    (request->action.kind == MASIL_ACTION_LAUNCH &&
	    request->action.launch_target != NULL && request->action.launch_staged)) {
		/*
		 * Unknown or expired IDs are request errors, before a ticket is
		 * consumed. Completion, digest, and key checks below instead belong to
		 * the admitted effect and are retained rejected-before-effect results.
		 */
		masil_action_staging_expire();
		masil_action_staging_schedule();
		if (request->action.kind == MASIL_ACTION_INPUT_COMMIT)
			staging = masil_action_staging_find(request->action.staging_id,
			    request->action.staging_id_length);
		else
			staging = masil_action_staging_find(
			    request->action.launch_spec_staging_id,
			    request->action.launch_spec_staging_id_length);
		if (staging == NULL)
			return (masil_action_response_error(response, response_size,
			    request_id, request_id_length, "staging_unknown"));
	}
	if (request->retain && (slot = masil_action_slot_allocate()) == NULL) {
		masil_action_active_next_seq++;
		if (staging != NULL) {
			masil_action_staging_release(staging);
			masil_action_staging_schedule();
		}
		return (masil_action_response_error(response, response_size,
		    request_id, request_id_length, "ledger_full"));
	}
	masil_action_active_next_seq++;
	if (request->retain)
		masil_action_slot_store(slot, request);
	if (staging != NULL)
		reason = masil_action_staging_check(staging, request);
	else
		reason = MASIL_ACTION_REASON_NONE;
	if (reason != MASIL_ACTION_REASON_NONE)
		result = MASIL_ACTION_RESULT_REJECTED;
	else {
		if (request->action.kind == MASIL_ACTION_LAUNCH &&
		    request->action.launch_target != NULL)
			reason = masil_action_check_launch_preconditions(request);
		else if (request->action.kind == MASIL_ACTION_FOCUS &&
		    request->action.focus_args)
			reason = masil_action_check_focus_preconditions(request, &wp);
		else
			reason = masil_action_check_preconditions(request, &wp);
		if (reason != MASIL_ACTION_REASON_NONE)
			result = MASIL_ACTION_RESULT_REJECTED;
		else
			result = masil_action_apply(request, wp, staging, &reason,
			    &outcome);
	}
	if (slot != NULL) {
		slot->result = result;
		slot->reason = reason;
		slot->queued_bytes = outcome.queued_bytes;
		slot->launch = outcome.launch;
		slot->pane_id = outcome.pane_id;
		slot->pid = outcome.pid;
		slot->pty_generation = outcome.pty_generation;
	}
	if (staging != NULL) {
		masil_action_staging_release(staging);
		masil_action_staging_schedule();
	}
	return (masil_action_response_result(response, response_size,
	    "guarded_action", request_id, request_id_length, &request->ticket,
	    result, reason, &outcome));
}

static int
masil_action_response_retired(char *response, size_t response_size,
    const char *request_id, size_t request_id_length,
    const struct masil_action_ticket *ticket)
{
	struct masil_action_json json = { response, 0, response_size, 0 };

	masil_action_json_puts(&json,
	    "{\"v\":1,\"kind\":\"retire_receipt\",\"request_id\":");
	masil_action_json_quote(&json, request_id, request_id_length);
	masil_action_json_printf(&json,
	    ",\"dispatch_ticket\":{\"epoch\":\"%llu\",\"seq\":\"%llu\"},\"retired\":true}",
	    (unsigned long long)ticket->epoch,
	    (unsigned long long)ticket->seq);
	return (masil_action_json_done(&json));
}

static int
masil_action_slot_compare_seq(const void *left, const void *right)
{
	const struct masil_action_slot *const *a = left;
	const struct masil_action_slot *const *b = right;

	if ((*a)->seq < (*b)->seq)
		return (-1);
	if ((*a)->seq > (*b)->seq)
		return (1);
	return (0);
}

static int
masil_action_response_list(char *response, size_t response_size,
    const char *request_id, size_t request_id_length, uint64_t epoch,
    uint64_t after_seq, int has_after_seq)
{
	struct masil_action_json json = { response, 0, response_size, 0 };
	struct masil_action_json entry_json;
	struct masil_action_ticket ticket;
	const struct masil_action_slot *slot, *slots[MASIL_ACTION_SLOTS];
	const unsigned char *operation_id, *key_value;
	unsigned char digest[32];
	char entry[MASIL_ACTION_LIST_ENTRY_BYTES];
	char key_digest[65], payload_digest[65], result_digest[65], result[128];
	size_t operation_id_length, entries = 0, returned = 0, i, needed;
	size_t component, key_value_length;
	uint64_t next_seq = has_after_seq ? after_seq : 0;
	int first = 1, truncated = 0;

	for (i = 0; i < MASIL_ACTION_SLOTS; i++) {
		slot = &masil_action_slots[i];
		if (!slot->used || slot->epoch != epoch ||
		    (has_after_seq && slot->seq <= after_seq))
			continue;
		slots[entries++] = slot;
	}
	qsort(slots, entries, sizeof *slots, masil_action_slot_compare_seq);

	masil_action_json_printf(&json,
	    "{\"v\":1,\"kind\":\"ledger_list\",\"request_id\":");
	masil_action_json_quote(&json, request_id, request_id_length);
	masil_action_json_printf(&json, ",\"epoch\":\"%llu\",\"entries\":[",
	    (unsigned long long)epoch);
	for (i = 0; i < entries; i++) {
		slot = slots[i];
		if (masil_action_slot_component(slot, 4, &operation_id,
		    &operation_id_length) != 0 || masil_action_result_text(slot->result,
		    slot->reason, result, sizeof result) != 0)
			return (masil_action_response_error(response, response_size,
			    request_id, request_id_length, "ledger_corrupt"));
		masil_action_sha256(result, strlen(result), digest);
		masil_action_digest_hex(digest, result_digest);
		masil_action_sha256(slot->key, slot->key_length, digest);
		masil_action_digest_hex(digest, key_digest);
		ticket.epoch = slot->epoch;
		ticket.seq = slot->seq;
		entry_json = (struct masil_action_json){ entry, 0, sizeof entry, 0 };
		masil_action_json_printf(&entry_json,
		    "{\"dispatch_ticket\":{\"epoch\":\"%llu\",\"seq\":\"%llu\"},\"operation_id\":",
		    (unsigned long long)ticket.epoch,
		    (unsigned long long)ticket.seq);
		masil_action_json_quote(&entry_json, (const char *)operation_id,
		    operation_id_length);
		masil_action_json_puts(&entry_json, ",\"operation_key_digest\":");
		masil_action_json_quote(&entry_json, key_digest, strlen(key_digest));
		/*
		 * The whole key, so a coordinator that lost its record can still
		 * retire the entry instead of leaving the slot pinned.
		 */
		masil_action_json_puts(&entry_json, ",\"operation_key\":{");
		for (component = 0; component < nitems(masil_action_key_fields);
		    component++) {
			if (masil_action_slot_component(slot, component,
			    &key_value, &key_value_length) != 0)
				return (masil_action_response_error(response,
				    response_size, request_id, request_id_length,
				    "ledger_corrupt"));
			if (component != 0)
				masil_action_json_puts(&entry_json, ",");
			masil_action_json_quote(&entry_json,
			    masil_action_key_fields[component],
			    strlen(masil_action_key_fields[component]));
			masil_action_json_puts(&entry_json, ":");
			masil_action_json_quote(&entry_json,
			    (const char *)key_value, key_value_length);
		}
		masil_action_json_puts(&entry_json, "}");
		masil_action_digest_hex(slot->payload_digest, payload_digest);
		masil_action_json_puts(&entry_json, ",\"payload_digest\":");
		masil_action_json_quote(&entry_json, payload_digest,
		    strlen(payload_digest));
		masil_action_json_puts(&entry_json, ",\"result\":");
		masil_action_json_quote(&entry_json, result, strlen(result));
		masil_action_json_puts(&entry_json, ",\"result_digest\":");
		masil_action_json_quote(&entry_json, result_digest,
		    strlen(result_digest));
		if (slot->queued_bytes != 0)
			masil_action_json_printf(&entry_json,
			    ",\"queued_bytes\":%u", slot->queued_bytes);
		if (slot->launch)
			masil_action_json_printf(&entry_json,
			    ",\"pane_id\":\"%%%u\",\"pid\":\"%ld\","
			    "\"pty_generation\":\"%llu\"", slot->pane_id,
			    (long)slot->pid,
			    (unsigned long long)slot->pty_generation);
		masil_action_json_puts(&entry_json, "}");
		if (masil_action_json_done(&entry_json) != 0)
			return (masil_action_response_error(response, response_size,
			    request_id, request_id_length, "ledger_corrupt"));
		needed = entry_json.len + (first ? 0 : 1);
		if (json.failed || json.len > json.cap ||
		    json.cap - json.len < MASIL_ACTION_LIST_TRAILER_BYTES ||
		    needed > json.cap - json.len - MASIL_ACTION_LIST_TRAILER_BYTES) {
			truncated = 1;
			break;
		}
		if (!first)
			masil_action_json_puts(&json, ",");
		masil_action_json_putn(&json, entry, entry_json.len);
		first = 0;
		next_seq = slot->seq;
		returned++;
	}
	if (returned != entries)
		truncated = 1;
	/* Keep the closing trailer outside the entry budget above. */
	masil_action_json_printf(&json,
	    "],\"next_seq\":\"%llu\",\"truncated\":%s}",
	    (unsigned long long)next_seq, truncated ? "true" : "false");
	return (masil_action_json_done(&json));
}

static int
masil_action_response_epochs(char *response, size_t response_size,
    const char *request_id, size_t request_id_length, uint64_t after_epoch,
    int has_after_epoch)
{
	struct masil_action_json json = { response, 0, response_size, 0 };
	struct masil_action_json entry_json;
	struct masil_action_closed_epoch current;
	const struct masil_action_closed_epoch
	    *epochs[MASIL_ACTION_SLOTS + 1], *closed;
	char entry[MASIL_ACTION_EPOCH_ENTRY_BYTES];
	size_t entries = 0, returned = 0, i, needed;
	uint64_t next_epoch = has_after_epoch ? after_epoch : 0;
	int first = 1, truncated = 0;

	for (i = 0; i < masil_action_closed_count; i++) {
		closed = &masil_action_closed_epochs[i];
		if (closed->slots_used == 0 ||
		    (has_after_epoch && closed->epoch <= after_epoch))
			continue;
		epochs[entries++] = closed;
	}
	if (masil_action_active_epoch != 0 &&
	    (!has_after_epoch || masil_action_active_epoch > after_epoch)) {
		current.epoch = masil_action_active_epoch;
		current.watermark = masil_action_active_next_seq;
		current.slots_used = masil_action_active_slots_used;
		epochs[entries++] = &current;
	}

	masil_action_json_puts(&json,
	    "{\"v\":1,\"kind\":\"ledger_epochs\",\"request_id\":");
	masil_action_json_quote(&json, request_id, request_id_length);
	masil_action_json_puts(&json, ",\"epochs\":[");
	for (i = 0; i < entries; i++) {
		closed = epochs[i];
		entry_json = (struct masil_action_json){ entry, 0, sizeof entry, 0 };
		masil_action_json_printf(&entry_json,
		    "{\"epoch\":\"%llu\",\"watermark\":\"%llu\",\"retained\":\"%zu\"}",
		    (unsigned long long)closed->epoch,
		    (unsigned long long)closed->watermark, closed->slots_used);
		if (masil_action_json_done(&entry_json) != 0)
			return (masil_action_response_error(response, response_size,
			    request_id, request_id_length, "ledger_corrupt"));
		needed = entry_json.len + (first ? 0 : 1);
		if (json.failed || json.len > json.cap ||
		    json.cap - json.len < MASIL_ACTION_LIST_TRAILER_BYTES ||
		    needed > json.cap - json.len - MASIL_ACTION_LIST_TRAILER_BYTES) {
			truncated = 1;
			break;
		}
		if (!first)
			masil_action_json_puts(&json, ",");
		masil_action_json_putn(&json, entry, entry_json.len);
		first = 0;
		next_epoch = closed->epoch;
		returned++;
	}
	if (returned != entries)
		truncated = 1;
	masil_action_json_printf(&json,
	    "],\"next_epoch\":\"%llu\",\"truncated\":%s}",
	    (unsigned long long)next_epoch, truncated ? "true" : "false");
	return (masil_action_json_done(&json));
}

static int
masil_action_guarded_request(uint64_t connection_epoch,
    const char *request_id, size_t request_id_length, yyjson_val *root,
    char *response, size_t response_size)
{
	static const char *const fields[] = {
		"v", "kind", "request_id", "operation_key", "dispatch_ticket",
		"payload_digest", "retain", "target", "preconditions", "action"
	};
	struct masil_action_request request;
	yyjson_val *retain;

	if (masil_action_schema(root, fields, nitems(fields)) != 0)
		return (masil_action_response_error(response, response_size,
		    request_id, request_id_length, "invalid_request"));
	memset(&request, 0, sizeof request);
	if (masil_action_parse_operation_key(yyjson_obj_get(root,
	    "operation_key"), &request.operation_key) != 0)
		return (masil_action_response_error(response, response_size,
		    request_id, request_id_length, "invalid_operation_key"));
	if (masil_action_parse_ticket(yyjson_obj_get(root, "dispatch_ticket"),
	    &request.ticket) != 0)
		return (masil_action_response_error(response, response_size,
		    request_id, request_id_length, "invalid_dispatch_ticket"));
	if (masil_action_parse_payload_digest(yyjson_obj_get(root,
	    "payload_digest"), request.payload_digest) != 0)
		return (masil_action_response_error(response, response_size,
		    request_id, request_id_length, "invalid_payload_digest"));
	retain = yyjson_obj_get(root, "retain");
	if (!yyjson_is_bool(retain))
		return (masil_action_response_error(response, response_size,
		    request_id, request_id_length, "invalid_request"));
	request.retain = yyjson_get_bool(retain);
	if (masil_action_parse_action(yyjson_obj_get(root, "action"),
	    &request.action) != 0)
		return (masil_action_response_error(response, response_size,
		    request_id, request_id_length, "invalid_action"));
	if (request.action.kind == MASIL_ACTION_LAUNCH &&
	    request.action.launch_target != NULL) {
		if (!request.retain)
			return (masil_action_response_error(response, response_size,
			    request_id, request_id_length, "invalid_request"));
		if (masil_action_parse_launch_target(yyjson_obj_get(root, "target"),
		    &request.action, &request.target) != 0)
			return (masil_action_response_error(response, response_size,
			    request_id, request_id_length, "invalid_target"));
		if (masil_action_launch_preconditions_valid(yyjson_obj_get(root,
		    "preconditions")) != 0)
			return (masil_action_response_error(response, response_size,
			    request_id, request_id_length, "invalid_preconditions"));
	} else {
		if (masil_action_parse_target(yyjson_obj_get(root, "target"),
		    &request.target) != 0)
			return (masil_action_response_error(response, response_size,
			    request_id, request_id_length, "invalid_target"));
		if ((request.action.kind == MASIL_ACTION_FOCUS &&
		    request.action.focus_args ?
		    masil_action_parse_focus_preconditions(yyjson_obj_get(root,
		    "preconditions"), &request.preconditions) :
		    masil_action_parse_preconditions(yyjson_obj_get(root,
		    "preconditions"), &request.preconditions)) != 0)
			return (masil_action_response_error(response, response_size,
			    request_id, request_id_length, "invalid_preconditions"));
	}
	if (request.action.kind == MASIL_ACTION_INPUT_COMMIT &&
	    !request.action.input_legacy &&
	    !masil_action_input_preconditions_valid(&request))
		return (masil_action_response_error(response, response_size,
		    request_id, request_id_length, "invalid_preconditions"));
	return (masil_action_guarded_execute(connection_epoch, request_id,
	    request_id_length, &request, response, response_size));
}

static int
masil_action_response_input_begin(char *response, size_t response_size,
    const char *request_id, size_t request_id_length, const char *staging_id,
    size_t staging_id_length)
{
	struct masil_action_json json = { response, 0, response_size, 0 };

	masil_action_json_puts(&json,
	    "{\"v\":1,\"kind\":\"input_begin\",\"request_id\":");
	masil_action_json_quote(&json, request_id, request_id_length);
	masil_action_json_puts(&json, ",\"staging_id\":");
	masil_action_json_quote(&json, staging_id, staging_id_length);
	masil_action_json_puts(&json, "}");
	return (masil_action_json_done(&json));
}

static int
masil_action_response_input_chunk(char *response, size_t response_size,
    const char *request_id, size_t request_id_length, const char *staging_id,
    size_t staging_id_length, size_t received)
{
	struct masil_action_json json = { response, 0, response_size, 0 };

	masil_action_json_puts(&json,
	    "{\"v\":1,\"kind\":\"input_chunk\",\"request_id\":");
	masil_action_json_quote(&json, request_id, request_id_length);
	masil_action_json_puts(&json, ",\"staging_id\":");
	masil_action_json_quote(&json, staging_id, staging_id_length);
	masil_action_json_printf(&json, ",\"received\":%zu}", received);
	return (masil_action_json_done(&json));
}

static int
masil_action_input_epoch_matches(uint64_t connection_epoch, char *response,
    size_t response_size, const char *request_id, size_t request_id_length)
{
	uint64_t watermark;

	if (connection_epoch != 0 && connection_epoch == masil_action_active_epoch)
		return (0);
	if (masil_action_closed_watermark(connection_epoch, &watermark))
		return (masil_action_response_error(response, response_size, request_id,
		    request_id_length, "epoch_closed"));
	return (masil_action_response_error(response, response_size, request_id,
	    request_id_length, "epoch_unknown"));
}

static int
masil_action_input_begin_request(uint64_t connection_epoch,
    const char *request_id, size_t request_id_length, yyjson_val *root,
    char *response, size_t response_size)
{
	static const char *const fields[] = {
		"v", "kind", "request_id", "staging_id", "operation_key",
		"total_len", "sha256", "core_boot_id"
	};
	struct masil_action_staging_slot *slot;
	struct masil_action_key operation_key;
	const char *staging_id, *boot_id;
	size_t staging_id_length, boot_id_length;
	uint64_t total_len, now;
	unsigned char digest[32];
	char *data;
	int result;

	if ((result = masil_action_input_epoch_matches(connection_epoch, response,
	    response_size, request_id, request_id_length)) != 0)
		return (result);
	if (masil_action_schema(root, fields, nitems(fields)) != 0 ||
	    masil_action_parse_string(root, "staging_id", &staging_id,
	    &staging_id_length) != 0 || !masil_action_staging_id_valid(staging_id,
	    staging_id_length) || masil_action_parse_core_boot_id(root, &boot_id,
	    &boot_id_length) != 0 || masil_action_parse_u64(yyjson_obj_get(root,
	    "total_len"), &total_len) != 0 || total_len == 0 ||
	    total_len > MASIL_ACTION_STAGING_BYTES ||
	    masil_action_digest_decode(yyjson_obj_get(root, "sha256"), digest) != 0)
		return (masil_action_response_error(response, response_size,
		    request_id, request_id_length, "invalid_request"));
	if (!masil_action_core_boot_matches(boot_id, boot_id_length))
		return (masil_action_response_error(response, response_size,
		    request_id, request_id_length, "boot_mismatch"));
	if (masil_action_parse_operation_key(yyjson_obj_get(root, "operation_key"),
	    &operation_key) != 0)
		return (masil_action_response_error(response, response_size,
		    request_id, request_id_length, "invalid_operation_key"));
	masil_action_staging_expire();
	masil_action_staging_schedule();
	if (masil_action_staging_find(staging_id, staging_id_length) != NULL)
		return (masil_action_response_error(response, response_size,
		    request_id, request_id_length, "staging_conflict"));
	if ((slot = masil_action_staging_allocate()) == NULL)
		return (masil_action_response_error(response, response_size,
		    request_id, request_id_length, "staging_full"));
	data = slot->data;
	memset(slot, 0, sizeof *slot);
	slot->data = data;
	memcpy(slot->id, staging_id, staging_id_length);
	slot->operation_key = operation_key;
	memcpy(slot->expected_digest, digest, sizeof digest);
	slot->total_len = total_len;
	now = get_timer();
	slot->deadline = now > UINT64_MAX - MASIL_ACTION_STAGING_TTL ?
	    UINT64_MAX : now + MASIL_ACTION_STAGING_TTL;
	masil_action_sha256_init(&slot->sha256);
	slot->active = 1;
	masil_action_staging_schedule();
	return (masil_action_response_input_begin(response, response_size,
	    request_id, request_id_length, staging_id, staging_id_length));
}

static int
masil_action_input_chunk_request(uint64_t connection_epoch,
    const char *request_id, size_t request_id_length, yyjson_val *root,
    char *response, size_t response_size)
{
	static const char *const fields[] = {
		"v", "kind", "request_id", "staging_id", "offset", "data_b64"
	};
	struct masil_action_staging_slot *slot;
	const char *staging_id, *data_b64;
	size_t staging_id_length, data_b64_length;
	uint64_t offset;
	unsigned char data[MASIL_ACTION_STAGING_CHUNK_BYTES];
	int decoded, result;

	if ((result = masil_action_input_epoch_matches(connection_epoch, response,
	    response_size, request_id, request_id_length)) != 0)
		return (result);
	if (masil_action_schema(root, fields, nitems(fields)) != 0 ||
	    masil_action_parse_string(root, "staging_id", &staging_id,
	    &staging_id_length) != 0 || !masil_action_staging_id_valid(staging_id,
	    staging_id_length) || masil_action_parse_u64(yyjson_obj_get(root,
	    "offset"), &offset) != 0 || masil_action_parse_string(root,
	    "data_b64", &data_b64, &data_b64_length) != 0 ||
	    data_b64_length > MASIL_ACTION_STAGING_CHUNK_B64 ||
	    memchr(data_b64, '\0', data_b64_length) != NULL)
		return (masil_action_response_error(response, response_size,
		    request_id, request_id_length, "invalid_request"));
	masil_action_staging_expire();
	masil_action_staging_schedule();
	slot = masil_action_staging_find(staging_id, staging_id_length);
	if (slot == NULL)
		return (masil_action_response_error(response, response_size,
		    request_id, request_id_length, "staging_unknown"));
	if (offset != slot->received)
		return (masil_action_response_error(response, response_size,
		    request_id, request_id_length, "staging_offset"));
	decoded = b64_pton(data_b64, data, sizeof data);
	if (decoded <= 0 || decoded > MASIL_ACTION_STAGING_CHUNK_BYTES)
		return (masil_action_response_error(response, response_size,
		    request_id, request_id_length, "invalid_request"));
	if ((size_t)decoded > slot->total_len - slot->received)
		return (masil_action_response_error(response, response_size,
		    request_id, request_id_length, "staging_overflow"));
	memcpy(slot->data + slot->received, data, decoded);
	masil_action_sha256_update(&slot->sha256, data, decoded);
	slot->received += decoded;
	return (masil_action_response_input_chunk(response, response_size,
	    request_id, request_id_length, staging_id, staging_id_length,
	    slot->received));
}

static int
masil_action_retire_request(const char *request_id, size_t request_id_length,
    yyjson_val *root, char *response, size_t response_size)
{
	static const char *const fields[] = {
		"v", "kind", "request_id", "operation_key", "dispatch_ticket",
		"result_digest", "store_revision", "core_boot_id"
	};
	struct masil_action_key operation_key;
	struct masil_action_ticket ticket;
	struct masil_action_slot *slot;
	unsigned char supplied_digest[32], result_digest[32];
	char result[128];
	const char *boot_id;
	size_t boot_id_length;
	uint64_t watermark, store_revision;
	struct masil_action_outcome outcome = { 0 };

	if (masil_action_schema(root, fields, nitems(fields)) != 0 ||
	    masil_action_parse_core_boot_id(root, &boot_id, &boot_id_length) != 0 ||
	    masil_action_parse_operation_key(yyjson_obj_get(root, "operation_key"),
	    &operation_key) != 0 || masil_action_parse_ticket(yyjson_obj_get(root,
	    "dispatch_ticket"), &ticket) != 0 ||
	    masil_action_parse_u64(yyjson_obj_get(root, "store_revision"),
	    &store_revision) != 0)
		return (masil_action_response_error(response, response_size,
		    request_id, request_id_length, "invalid_request"));
	(void)store_revision;
	if (!masil_action_core_boot_matches(boot_id, boot_id_length))
		return (masil_action_response_error(response, response_size,
		    request_id, request_id_length, "boot_mismatch"));
	if (!masil_action_known_epoch(ticket.epoch, &watermark))
		return (masil_action_response_error(response, response_size,
		    request_id, request_id_length, "epoch_unknown"));
	if (ticket.seq >= watermark)
		return (masil_action_response_result(response, response_size,
		    "retire_receipt", request_id, request_id_length, &ticket,
		    MASIL_ACTION_RESULT_NOT_APPLIED, MASIL_ACTION_REASON_NONE,
		    &outcome));
	slot = masil_action_slot_find(&ticket);
	if (slot == NULL)
		return (masil_action_response_error(response, response_size,
		    request_id, request_id_length, "receipt_not_retained"));
	if (slot->key_length != operation_key.length ||
	    memcmp(slot->key, operation_key.bytes, slot->key_length) != 0 ||
	    masil_action_digest_decode(yyjson_obj_get(root, "result_digest"),
	    supplied_digest) != 0 || masil_action_result_text(slot->result,
	    slot->reason, result, sizeof result) != 0)
		return (masil_action_response_error(response, response_size,
		    request_id, request_id_length, "retire_mismatch"));
	masil_action_sha256(result, strlen(result), result_digest);
	if (memcmp(supplied_digest, result_digest, sizeof result_digest) != 0)
		return (masil_action_response_error(response, response_size,
		    request_id, request_id_length, "retire_mismatch"));
	masil_action_slot_release(slot);
	return (masil_action_response_retired(response, response_size, request_id,
	    request_id_length, &ticket));
}

static int
masil_action_query_request(const char *request_id, size_t request_id_length,
    yyjson_val *root, char *response, size_t response_size)
{
	static const char *const fields[] = {
		"v", "kind", "request_id", "dispatch_ticket", "core_boot_id"
	};
	struct masil_action_ticket ticket;
	struct masil_action_slot *slot;
	const char *boot_id;
	size_t boot_id_length;
	uint64_t watermark;
	struct masil_action_outcome outcome = { 0 };

	if (masil_action_schema(root, fields, nitems(fields)) != 0 ||
	    masil_action_parse_core_boot_id(root, &boot_id, &boot_id_length) != 0 ||
	    masil_action_parse_ticket(yyjson_obj_get(root, "dispatch_ticket"),
	    &ticket) != 0)
		return (masil_action_response_error(response, response_size,
		    request_id, request_id_length, "invalid_request"));
	if (!masil_action_core_boot_matches(boot_id, boot_id_length))
		return (masil_action_response_error(response, response_size,
		    request_id, request_id_length, "boot_mismatch"));
	if (!masil_action_known_epoch(ticket.epoch, &watermark))
		return (masil_action_response_error(response, response_size,
		    request_id, request_id_length, "epoch_unknown"));
	if (ticket.seq >= watermark)
		return (masil_action_response_result(response, response_size,
		    "ledger_query", request_id, request_id_length, &ticket,
		    MASIL_ACTION_RESULT_NOT_APPLIED, MASIL_ACTION_REASON_NONE,
		    &outcome));
	slot = masil_action_slot_find(&ticket);
	if (slot == NULL)
		return (masil_action_response_error(response, response_size,
		    request_id, request_id_length, "receipt_not_retained"));
	outcome.queued_bytes = slot->queued_bytes;
	outcome.launch = slot->launch;
	outcome.pane_id = slot->pane_id;
	outcome.pid = slot->pid;
	outcome.pty_generation = slot->pty_generation;
	return (masil_action_response_result(response, response_size,
	    "ledger_query", request_id, request_id_length, &ticket, slot->result,
	    slot->reason, &outcome));
}

static int
masil_action_list_request(const char *request_id, size_t request_id_length,
    yyjson_val *root, char *response, size_t response_size)
{
	static const char *const fields[] = {
		"v", "kind", "request_id", "epoch", "after_seq", "core_boot_id"
	};
	yyjson_val *after_seq_value;
	const char *boot_id;
	size_t boot_id_length;
	uint64_t after_seq = 0, epoch, watermark;
	int has_after_seq = 0;

	if (masil_action_schema(root, fields, nitems(fields)) != 0 ||
	    masil_action_parse_core_boot_id(root, &boot_id, &boot_id_length) != 0 ||
	    masil_action_parse_u64(yyjson_obj_get(root, "epoch"), &epoch) != 0 ||
	    epoch == 0)
		return (masil_action_response_error(response, response_size,
		    request_id, request_id_length, "invalid_request"));
	if (!masil_action_core_boot_matches(boot_id, boot_id_length))
		return (masil_action_response_error(response, response_size,
		    request_id, request_id_length, "boot_mismatch"));
	after_seq_value = yyjson_obj_get(root, "after_seq");
	if (after_seq_value != NULL) {
		if (!yyjson_is_str(after_seq_value) ||
		    masil_action_parse_u64(after_seq_value, &after_seq) != 0)
			return (masil_action_response_error(response, response_size,
			    request_id, request_id_length, "invalid_request"));
		has_after_seq = 1;
	}
	if (!masil_action_known_epoch(epoch, &watermark))
		return (masil_action_response_error(response, response_size,
		    request_id, request_id_length, "epoch_unknown"));
	return (masil_action_response_list(response, response_size, request_id,
	    request_id_length, epoch, after_seq, has_after_seq));
}

static int
masil_action_epochs_request(const char *request_id, size_t request_id_length,
    yyjson_val *root, char *response, size_t response_size)
{
	static const char *const fields[] = {
		"v", "kind", "request_id", "after_epoch", "core_boot_id"
	};
	yyjson_val *after_epoch_value;
	const char *boot_id;
	size_t boot_id_length;
	uint64_t after_epoch = 0;
	int has_after_epoch = 0;

	if (masil_action_schema(root, fields, nitems(fields)) != 0 ||
	    masil_action_parse_core_boot_id(root, &boot_id, &boot_id_length) != 0)
		return (masil_action_response_error(response, response_size,
		    request_id, request_id_length, "invalid_request"));
	if (!masil_action_core_boot_matches(boot_id, boot_id_length))
		return (masil_action_response_error(response, response_size,
		    request_id, request_id_length, "boot_mismatch"));
	after_epoch_value = yyjson_obj_get(root, "after_epoch");
	if (after_epoch_value != NULL) {
		if (!yyjson_is_str(after_epoch_value) ||
		    masil_action_parse_u64(after_epoch_value, &after_epoch) != 0)
			return (masil_action_response_error(response, response_size,
			    request_id, request_id_length, "invalid_request"));
		has_after_epoch = 1;
	}
	return (masil_action_response_epochs(response, response_size, request_id,
	    request_id_length, after_epoch, has_after_epoch));
}

/* masil: bridge owns connection authorization; this dispatch owns ledger state. */
int
masil_action_dispatch(uint64_t connection_epoch, const char *request_id,
    size_t request_id_length, const char *kind, struct yyjson_val *root,
    char *response, size_t response_size)
{
	if (strcmp(kind, "guarded_action") == 0)
		return (masil_action_guarded_request(connection_epoch, request_id,
		    request_id_length, root, response, response_size));
	if (strcmp(kind, "input_begin") == 0)
		return (masil_action_input_begin_request(connection_epoch, request_id,
		    request_id_length, root, response, response_size));
	if (strcmp(kind, "input_chunk") == 0)
		return (masil_action_input_chunk_request(connection_epoch, request_id,
		    request_id_length, root, response, response_size));
	if (strcmp(kind, "retire_receipt") == 0)
		return (masil_action_retire_request(request_id, request_id_length,
		    root, response, response_size));
	if (strcmp(kind, "ledger_query") == 0)
		return (masil_action_query_request(request_id, request_id_length, root,
		    response, response_size));
	if (strcmp(kind, "ledger_list") == 0)
		return (masil_action_list_request(request_id, request_id_length, root,
		    response, response_size));
	if (strcmp(kind, "ledger_epochs") == 0)
		return (masil_action_epochs_request(request_id, request_id_length, root,
		    response, response_size));
	return (masil_action_response_error(response, response_size, request_id,
	    request_id_length, "unsupported_kind"));
}
