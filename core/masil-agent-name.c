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
#ifdef __APPLE__
#include <sys/sysctl.h>
#endif

#include <ctype.h>
#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>

#include "tmux.h"

#define MASIL_AGENT_ARGV_BYTES	(128 * 1024)
#define MASIL_AGENT_ARGV_COUNT	128

struct masil_agent_argv {
	char	*buffer;
	char	*argv[MASIL_AGENT_ARGV_COUNT];
	size_t	 argc;
};

struct masil_agent_executable {
	const char	*name;
	const char	*label;
};

static const struct masil_agent_executable masil_agent_executables[] = {
	{ "pi", "Pi" },
	{ "claude", "Claude Code" },
	{ "claude-code", "Claude Code" },
	{ "codex", "Codex" },
	{ "gemini", "Gemini" },
	{ "cursor", "Cursor Agent" },
	{ "cursor-agent", "Cursor Agent" },
	{ "devin", "Devin" },
	{ "devin-cli", "Devin" },
	{ "agy", "Antigravity" },
	{ "antigravity", "Antigravity" },
	{ "antigravity-cli", "Antigravity" },
	{ "cline", "Cline" },
	{ ".cline", "Cline" },
	{ "omp", "Oh My Pi" },
	{ "mastracode", "Mastra Code" },
	{ "mastra-code", "Mastra Code" },
	{ "opencode", "OpenCode" },
	{ "opencode2", "OpenCode" },
	{ "open-code", "OpenCode" },
	{ "copilot", "GitHub Copilot" },
	{ "github-copilot", "GitHub Copilot" },
	{ "ghcs", "GitHub Copilot" },
	{ "kimi", "Kimi Code" },
	{ "kimi-code", "Kimi Code" },
	{ "kiro", "Kiro CLI" },
	{ "kiro-cli", "Kiro CLI" },
	{ "droid", "Droid" },
	{ "amp", "Amp" },
	{ "amp-local", "Amp" },
	{ "grok", "Grok" },
	{ "grok-build", "Grok" },
	{ "hermes", "Hermes" },
	{ "hermes-agent", "Hermes" },
	{ "kilo", "Kilo Code" },
	{ "kilo-code", "Kilo Code" },
	{ "qodercli", "Qoder CLI" },
	{ "qoderclicn", "Qoder CLI" },
	{ "qoder", "Qoder CLI" },
	{ "qodercn", "Qoder CLI" },
	{ "qwen", "Qwen Code" },
	{ "qwen-code", "Qwen Code" },
	{ "letta", "Letta Code" },
	{ "letta-code", "Letta Code" },
	{ "maki", "Maki" },
	{ "muse", "Muse" },
	{ "muse-code", "Muse" },
	{ "muse-cli", "Muse" }
};

static void
masil_agent_argv_free(struct masil_agent_argv *args)
{
	free(args->buffer);
	args->buffer = NULL;
	args->argc = 0;
}

static int
masil_agent_argv_split(struct masil_agent_argv *args, char *start, char *end,
    size_t limit)
{
	char	*next;

	while (start < end && args->argc < limit) {
		next = memchr(start, '\0', (size_t)(end - start));
		if (next == NULL)
			return (0);
		args->argv[args->argc++] = start;
		start = next + 1;
	}
	return (args->argc != 0);
}

#ifdef __APPLE__
static int
masil_agent_get_argv(pid_t pid, struct masil_agent_argv *args)
{
	int	 mib[3] = { CTL_KERN, KERN_PROCARGS2, 0 };
	int	 process_argc;
	char	*start, *end;
	size_t	 size = 0;

	mib[2] = pid;
	if (sysctl(mib, 3, NULL, &size, NULL, 0) == -1 || size == 0 ||
	    size > MASIL_AGENT_ARGV_BYTES)
		return (0);
	args->buffer = xmalloc(size);
	if (sysctl(mib, 3, args->buffer, &size, NULL, 0) == -1 ||
	    size <= sizeof process_argc)
		goto fail;

	memcpy(&process_argc, args->buffer, sizeof process_argc);
	if (process_argc <= 0)
		goto fail;
	start = args->buffer + sizeof process_argc;
	end = args->buffer + size;

	/* Skip the executable path, then the padding before argv[0]. */
	start = memchr(start, '\0', (size_t)(end - start));
	if (start == NULL)
		goto fail;
	while (start < end && *start == '\0')
		start++;
	if (!masil_agent_argv_split(args, start, end,
	    (size_t)process_argc < MASIL_AGENT_ARGV_COUNT ?
	    (size_t)process_argc : MASIL_AGENT_ARGV_COUNT))
		goto fail;
	return (1);

fail:
	masil_agent_argv_free(args);
	return (0);
}
#elif defined(__linux__)
static int
masil_agent_get_argv(pid_t pid, struct masil_agent_argv *args)
{
	char	 path[64];
	ssize_t	 n;
	size_t	 used = 0;
	int	 fd;

	if (snprintf(path, sizeof path, "/proc/%lld/cmdline",
	    (long long)pid) < 0)
		return (0);
	if ((fd = open(path, O_RDONLY|O_CLOEXEC)) == -1)
		return (0);
	args->buffer = xmalloc(MASIL_AGENT_ARGV_BYTES + 1);
	for (;;) {
		n = read(fd, args->buffer + used,
		    MASIL_AGENT_ARGV_BYTES + 1 - used);
		if (n == -1 && errno == EINTR)
			continue;
		if (n <= 0)
			break;
		used += (size_t)n;
		if (used > MASIL_AGENT_ARGV_BYTES)
			break;
	}
	close(fd);
	if (n == -1 || used == 0 || used > MASIL_AGENT_ARGV_BYTES ||
	    args->buffer[used - 1] != '\0')
		goto fail;
	if (!masil_agent_argv_split(args, args->buffer,
	    args->buffer + used, MASIL_AGENT_ARGV_COUNT))
		goto fail;
	return (1);

fail:
	masil_agent_argv_free(args);
	return (0);
}
#else
static int
masil_agent_get_argv(__unused pid_t pid, __unused struct masil_agent_argv *args)
{
	return (0);
}
#endif

static int
masil_agent_name_equal(const char *actual, size_t actual_len,
    const char *expected)
{
	return (strlen(expected) == actual_len &&
	    strncasecmp(actual, expected, actual_len) == 0);
}

static const char *
masil_agent_executable_name(const char *path)
{
	static const char	*const suffixes[] = {
		".exe", ".cmd", ".bat", ".ps1", ".js"
	};
	const char		*base, *slash, *backslash;
	const char		*suffix;
	size_t			 len, i;

	slash = strrchr(path, '/');
	backslash = strrchr(path, '\\');
	base = path;
	if (slash != NULL && (backslash == NULL || slash > backslash))
		base = slash + 1;
	else if (backslash != NULL)
		base = backslash + 1;
	len = strlen(base);
	for (i = 0; i < nitems(suffixes); i++) {
		suffix = suffixes[i];
		if (len > strlen(suffix) &&
		    strcasecmp(base + len - strlen(suffix), suffix) == 0) {
			len -= strlen(suffix);
			break;
		}
	}
	for (i = 0; i < nitems(masil_agent_executables); i++) {
		if (masil_agent_name_equal(base, len,
		    masil_agent_executables[i].name))
			return (masil_agent_executables[i].label);
	}
	if (len > (sizeof "muse-bin-") - 1 &&
	    strncasecmp(base, "muse-bin-", (sizeof "muse-bin-") - 1) == 0 &&
	    isdigit((u_char)base[(sizeof "muse-bin-") - 1]))
		return ("Muse");
	return (NULL);
}

static int
masil_agent_path_ends_with(const char *path, const char *suffix)
{
	const char	*p = path + strlen(path), *s = suffix + strlen(suffix);
	char		 pc, sc;

	while (s != suffix) {
		if (p == path)
			return (0);
		pc = *--p;
		sc = *--s;
		if ((pc == '/' || pc == '\\') && (sc == '/' || sc == '\\'))
			continue;
		if (tolower((u_char)pc) != tolower((u_char)sc))
			return (0);
	}
	return (p == path || p[-1] == '/' || p[-1] == '\\');
}

static const char *
masil_agent_script_name(const char *path)
{
	const char	*label;

	if ((label = masil_agent_executable_name(path)) != NULL)
		return (label);
	if (masil_agent_path_ends_with(path,
	    "node_modules/@earendil-works/pi-coding-agent/dist/cli.js") ||
	    masil_agent_path_ends_with(path,
	    "node_modules/@earendil-works/pi-coding-agent/dist/bundle/cli.js"))
		return ("Pi");
	if (masil_agent_path_ends_with(path,
	    "node_modules/@oh-my-pi/pi-coding-agent/dist/cli.js"))
		return ("Oh My Pi");
	if (masil_agent_path_ends_with(path,
	    "node_modules/@moonshot-ai/kimi-code/dist/main.mjs"))
		return ("Kimi Code");
	if (masil_agent_path_ends_with(path,
	    "node_modules/@qwen-code/qwen-code/dist/index.js") ||
	    masil_agent_path_ends_with(path,
	    "node_modules/@qwen-code/qwen-code/dist/index"))
		return ("Qwen Code");
	if (masil_agent_path_ends_with(path,
	    "node_modules/mastracode/dist/cli.js") ||
	    masil_agent_path_ends_with(path,
	    "node_modules/mastracode/dist/cli"))
		return ("Mastra Code");
	if (masil_agent_path_ends_with(path,
	    "node_modules/@letta-ai/letta-code/letta.js") ||
	    masil_agent_path_ends_with(path,
	    "node_modules/@letta-ai/letta-code/letta"))
		return ("Letta Code");
	return (NULL);
}

static int
masil_agent_flag_matches(const char *arg, const char *flag)
{
	size_t	 len = strlen(flag);

	if (strcmp(arg, flag) == 0)
		return (1);
	if (flag[1] != '-' && strncmp(arg, flag, len) == 0 && arg[len] != '\0')
		return (1);
	return (flag[1] == '-' && strncmp(arg, flag, len) == 0 &&
	    arg[len] == '=');
}

static int
masil_agent_option_takes_value(const char *arg)
{
	static const char	*const options[] = {
		"-r", "--require", "--loader", "--import",
		"--experimental-loader", "--inspect-port", "-W", "-X",
		"-S", "-L", "-o"
	};
	size_t			 i;

	for (i = 0; i < nitems(options); i++) {
		if (strcmp(arg, options[i]) == 0)
			return (1);
	}
	return (0);
}

static const char *
masil_agent_runtime_script(struct masil_agent_argv *args, int python)
{
	const char	*arg;
	size_t		 i = 1;

	while (i < args->argc) {
		arg = args->argv[i];
		if (strcmp(arg, "--") == 0)
			return (i + 1 < args->argc ? args->argv[i + 1] : NULL);
		if (python) {
			if (masil_agent_flag_matches(arg, "-c") ||
			    masil_agent_flag_matches(arg, "-m"))
				return (NULL);
		} else if (masil_agent_flag_matches(arg, "-e") ||
		    masil_agent_flag_matches(arg, "--eval") ||
		    masil_agent_flag_matches(arg, "-p") ||
		    masil_agent_flag_matches(arg, "--print"))
			return (NULL);
		if (*arg == '-') {
			i += masil_agent_option_takes_value(arg) ? 2 : 1;
			continue;
		}
		return (arg);
	}
	return (NULL);
}

static int
masil_agent_is_python(const char *name)
{
	const char	*version;

	if (strcmp(name, "python") == 0)
		return (1);
	if (strncmp(name, "python", (sizeof "python") - 1) != 0)
		return (0);
	version = name + (sizeof "python") - 1;
	if (!isdigit((u_char)*version))
		return (0);
	while (*version != '\0') {
		if (isdigit((u_char)*version)) {
			version++;
			continue;
		}
		if (*version != '.' || !isdigit((u_char)version[1]))
			return (0);
		version++;
	}
	return (1);
}

static int
masil_agent_runtime_name(const char *path, char *name, size_t name_size)
{
	const char	*base, *slash, *backslash;
	size_t		 len, i;

	slash = strrchr(path, '/');
	backslash = strrchr(path, '\\');
	base = path;
	if (slash != NULL && (backslash == NULL || slash > backslash))
		base = slash + 1;
	else if (backslash != NULL)
		base = backslash + 1;
	len = strlen(base);
	if (len == 0 || len >= name_size)
		return (0);
	for (i = 0; i < len; i++)
		name[i] = tolower((u_char)base[i]);
	name[len] = '\0';
	return (1);
}

const char *
masil_agent_name(int fd)
{
	struct masil_agent_argv	 args = { 0 };
	const char		*label = NULL, *script;
	char			 runtime[64];
	pid_t			 pid;

	if (fd == -1 || (pid = tcgetpgrp(fd)) <= 0 ||
	    !masil_agent_get_argv(pid, &args))
		return (NULL);
	if ((label = masil_agent_executable_name(args.argv[0])) != NULL)
		goto done;
	if (!masil_agent_runtime_name(args.argv[0], runtime, sizeof runtime))
		goto done;
	if (strcmp(runtime, "node") == 0 || strcmp(runtime, "nodejs") == 0 ||
	    strcmp(runtime, "bun") == 0) {
		script = masil_agent_runtime_script(&args, 0);
		if (script != NULL)
			label = masil_agent_script_name(script);
	} else if (masil_agent_is_python(runtime)) {
		script = masil_agent_runtime_script(&args, 1);
		if (script != NULL)
			label = masil_agent_script_name(script);
	}

done:
	masil_agent_argv_free(&args);
	return (label);
}
