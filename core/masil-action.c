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
#define MASIL_ACTION_PAYLOAD_DIGEST_BYTES 64
#define MASIL_ACTION_KEY_BYTES		171
#define MASIL_ACTION_CLOSED_EPOCHS	8
#define MASIL_ACTION_CLOSED_EPOCH_CAPACITY \
	(MASIL_ACTION_SLOTS + MASIL_ACTION_CLOSED_EPOCHS)
#define MASIL_ACTION_MAX_KEYS		64
#define MASIL_ACTION_LIST_ENTRY_BYTES	4096
#define MASIL_ACTION_EPOCH_ENTRY_BYTES	128
#define MASIL_ACTION_LIST_TRAILER_BYTES	128

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
	MASIL_ACTION_REASON_WINDOW_UNLINKED
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
	unsigned char	 payload_digest[MASIL_ACTION_PAYLOAD_DIGEST_BYTES];
	uint8_t		 used;
	uint8_t		 result;
	uint8_t		 reason;
	uint8_t		 key_length;
	uint8_t		 payload_digest_length;
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

struct masil_action_data {
	enum masil_action_kind kind;
	yyjson_val		*keys;
};

struct masil_action_request {
	struct masil_action_key		 operation_key;
	struct masil_action_ticket	 ticket;
	unsigned char			 payload_digest[MASIL_ACTION_PAYLOAD_DIGEST_BYTES];
	size_t				 payload_digest_length;
	int				 retain;
	struct masil_action_target	 target;
	struct masil_action_preconditions preconditions;
	struct masil_action_data		 action;
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

static struct masil_action_slot *masil_action_slots;
static size_t masil_action_slots_used;
static size_t masil_action_slot_cursor;
static uint64_t masil_action_next_epoch;
static uint64_t masil_action_active_epoch;
static uint64_t masil_action_active_next_seq;
static size_t masil_action_active_slots_used;
static struct masil_action_closed_epoch *masil_action_closed_epochs;
static size_t masil_action_closed_count;

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
	struct masil_action_sha256_ctx ctx = {
		{ 0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a,
		  0x510e527f, 0x9b05688c, 0x1f83d9ab, 0x5be0cd19 },
		0, { 0 }, 0
	};

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

/* masil: payload digests are opaque, bounded bytes rather than a C1a hash type. */
static int
masil_action_parse_payload_digest(yyjson_val *value,
    unsigned char digest[MASIL_ACTION_PAYLOAD_DIGEST_BYTES], size_t *length)
{
	const char *string;

	if (!yyjson_is_str(value))
		return (-1);
	string = yyjson_get_str(value);
	*length = yyjson_get_len(value);
	if (*length == 0 || *length > MASIL_ACTION_PAYLOAD_DIGEST_BYTES)
		return (-1);
	memcpy(digest, string, *length);
	return (0);
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
    enum masil_action_reason reason)
{
	struct masil_action_json json = { response, 0, response_size, 0 };
	unsigned char digest[32];
	char result_text[128], digest_text[65];

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
masil_action_parse_action(yyjson_val *value, struct masil_action_data *action)
{
	static const char *const keys_fields[] = { "kind", "type", "keys" };
	static const char *const no_argument_fields[] = { "kind", "type" };
	yyjson_val *kind, *type;
	const char *string;
	size_t length;

	if (yyjson_is_str(value)) {
		string = yyjson_get_str(value);
		length = yyjson_get_len(value);
		action->kind = masil_action_kind_from_string(string, length);
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
	}
	return (0);
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
	    request->payload_digest_length);
	slot->payload_digest_length = request->payload_digest_length;
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
    const unsigned char *payload_digest, size_t payload_digest_length)
{
	return (slot->key_length == operation_key->length &&
	    memcmp(slot->key, operation_key->bytes, slot->key_length) == 0 &&
	    slot->payload_digest_length == payload_digest_length &&
	    memcmp(slot->payload_digest, payload_digest, payload_digest_length) == 0);
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
masil_action_parse_pane_id(const char *value, size_t length, u_int *pane_id)
{
	uint64_t id = 0, digit;
	size_t i;

	if (length < 2 || value[0] != '%')
		return (-1);
	for (i = 1; i < length; i++) {
		if (value[i] < '0' || value[i] > '9')
			return (-1);
		digit = value[i] - '0';
		if (id > (UINT_MAX - digit) / 10)
			return (-1);
		id = id * 10 + digit;
	}
	*pane_id = id;
	return (0);
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

static enum masil_action_result
masil_action_apply(const struct masil_action_request *request,
    struct window_pane *wp, enum masil_action_reason *reason)
{
	struct session *session;
	struct winlink *winlink;
	key_code keys[MASIL_ACTION_MAX_KEYS];
	size_t count, i;

	*reason = MASIL_ACTION_REASON_NONE;
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
	case MASIL_ACTION_LAUNCH:
	case MASIL_ACTION_FOCUS:
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
	struct window_pane *wp;
	enum masil_action_result result;
	enum masil_action_reason reason;
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
		    request->payload_digest, request->payload_digest_length))
			return (masil_action_response_error(response, response_size,
			    request_id, request_id_length, "idempotency_conflict"));
		return (masil_action_response_result(response, response_size,
		    "guarded_action", request_id, request_id_length, &request->ticket,
		    slot->result, slot->reason));
	}
	if (request->ticket.seq > masil_action_active_next_seq)
		return (masil_action_response_error(response, response_size,
		    request_id, request_id_length, "ticket_gap"));
	if (masil_action_active_next_seq == UINT64_MAX)
		return (masil_action_response_error(response, response_size,
		    request_id, request_id_length, "sequence_exhausted"));
	if (request->retain && (slot = masil_action_slot_allocate()) == NULL) {
		masil_action_active_next_seq++;
		return (masil_action_response_error(response, response_size,
		    request_id, request_id_length, "ledger_full"));
	}
	masil_action_active_next_seq++;
	if (request->retain)
		masil_action_slot_store(slot, request);
	reason = masil_action_check_preconditions(request, &wp);
	if (reason != MASIL_ACTION_REASON_NONE)
		result = MASIL_ACTION_RESULT_REJECTED;
	else
		result = masil_action_apply(request, wp, &reason);
	if (slot != NULL) {
		slot->result = result;
		slot->reason = reason;
	}
	return (masil_action_response_result(response, response_size,
	    "guarded_action", request_id, request_id_length, &request->ticket,
	    result, reason));
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
	char key_digest[65], result_digest[65], result[128];
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
		masil_action_json_puts(&entry_json, ",\"payload_digest\":");
		masil_action_json_quote(&entry_json, (const char *)slot->payload_digest,
		    slot->payload_digest_length);
		masil_action_json_puts(&entry_json, ",\"result\":");
		masil_action_json_quote(&entry_json, result, strlen(result));
		masil_action_json_puts(&entry_json, ",\"result_digest\":");
		masil_action_json_quote(&entry_json, result_digest,
		    strlen(result_digest));
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
	    "payload_digest"), request.payload_digest,
	    &request.payload_digest_length) != 0)
		return (masil_action_response_error(response, response_size,
		    request_id, request_id_length, "invalid_payload_digest"));
	retain = yyjson_obj_get(root, "retain");
	if (!yyjson_is_bool(retain))
		return (masil_action_response_error(response, response_size,
		    request_id, request_id_length, "invalid_request"));
	request.retain = yyjson_get_bool(retain);
	if (masil_action_parse_target(yyjson_obj_get(root, "target"),
	    &request.target) != 0)
		return (masil_action_response_error(response, response_size,
		    request_id, request_id_length, "invalid_target"));
	if (masil_action_parse_preconditions(yyjson_obj_get(root, "preconditions"),
	    &request.preconditions) != 0)
		return (masil_action_response_error(response, response_size,
		    request_id, request_id_length, "invalid_preconditions"));
	if (masil_action_parse_action(yyjson_obj_get(root, "action"),
	    &request.action) != 0)
		return (masil_action_response_error(response, response_size,
		    request_id, request_id_length, "invalid_action"));
	return (masil_action_guarded_execute(connection_epoch, request_id,
	    request_id_length, &request, response, response_size));
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
		    MASIL_ACTION_RESULT_NOT_APPLIED, MASIL_ACTION_REASON_NONE));
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
		    MASIL_ACTION_RESULT_NOT_APPLIED, MASIL_ACTION_REASON_NONE));
	slot = masil_action_slot_find(&ticket);
	if (slot == NULL)
		return (masil_action_response_error(response, response_size,
		    request_id, request_id_length, "receipt_not_retained"));
	return (masil_action_response_result(response, response_size,
	    "ledger_query", request_id, request_id_length, &ticket, slot->result,
	    slot->reason));
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
