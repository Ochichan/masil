/* $OpenBSD: cmd-new-window.c,v 1.104 2026/09/03 21:04:11 nicm Exp $ */

/*
 * Copyright (c) 2007 Nicholas Marriott <nicholas.marriott@gmail.com>
 *
 * Permission to use, copy, modify, and distribute this software for any
 * purpose with or without fee is hereby granted, provided that the above
 * copyright notice and this permission notice appear in all copies.
 *
 * THE SOFTWARE IS PROVIDED "AS IS" AND THE AUTHOR DISCLAIMS ALL WARRANTIES
 * WITH REGARD TO THIS SOFTWARE INCLUDING ALL IMPLIED WARRANTIES OF
 * MERCHANTABILITY AND FITNESS. IN NO EVENT SHALL THE AUTHOR BE LIABLE FOR
 * ANY SPECIAL, DIRECT, INDIRECT, OR CONSEQUENTIAL DAMAGES OR ANY DAMAGES
 * WHATSOEVER RESULTING FROM LOSS OF MIND, USE, DATA OR PROFITS, WHETHER
 * IN AN ACTION OF CONTRACT, NEGLIGENCE OR OTHER TORTIOUS ACTION, ARISING
 * OUT OF OR IN CONNECTION WITH THE USE OR PERFORMANCE OF THIS SOFTWARE.
 */

#include <sys/types.h>

#include <errno.h>
#include <fcntl.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>

#include "tmux.h"

/*
 * Create a new window.
 */

#define NEW_WINDOW_TEMPLATE "#{session_name}:#{window_index}.#{pane_index}"

static enum cmd_retval	cmd_new_window_exec(struct cmd *, struct cmdq_item *);

const struct cmd_entry cmd_new_window_entry = {
	.name = "new-window",
	.alias = "neww",

	.args = { "abc:de:EF:kn:PSt:", 0, -1, NULL },
	.usage = "[-abdEkPS] [-c start-directory] [-e environment] [-F format] "
		 "[-n window-name] " CMD_TARGET_WINDOW_USAGE
		 " [shell-command [argument ...]]",

	.target = { 't', CMD_FIND_WINDOW, CMD_FIND_WINDOW_INDEX },

	.flags = 0,
	.exec = cmd_new_window_exec
};

static enum cmd_retval
cmd_new_window_exec(struct cmd *self, struct cmdq_item *item)
{
	struct args		*args = cmd_get_args(self);
	struct client		*c = cmdq_get_client(item);
	struct cmd_find_state	*current = cmdq_get_current(item);
	struct cmd_find_state	*target = cmdq_get_target(item);
	struct spawn_context	 sc = { 0 };
	struct client		*tc = cmdq_get_target_client(item);
	struct session		*s = target->s;
	struct winlink		*wl = target->wl, *new_wl = NULL;
	int			 idx = target->idx, before;
	int			 count = args_count(args);
	char			*cause = NULL, *cp, *expanded, *wname = NULL;
	const char		*template, *name;
	struct cmd_find_state	 fs;
	struct args_value	*av;
	/* masil: C3 context is present only on fixed guarded launches. */
	struct masil_launch_context *masil_launch = cmdq_get_masil_launch(item);

	if (masil_launch != NULL) {
		masil_launch_context_command_started(masil_launch);
		if (masil_launch_context_cancelled(masil_launch)) {
			/* masil: never let a bridge-timeout launch run after queue resume. */
			cmdq_error(item, "launch command cancelled");
			return (CMD_RETURN_ERROR);
		}
	}

	if (args_has(args, 'E') &&
	    count != 0 &&
	    (count != 1 || *args_string(args, 0) != '\0')) {
		cmdq_error(item, "command cannot be given for empty pane");
		return (CMD_RETURN_ERROR);
	}

	/*
	 * If -S is given, select an existing window instead of creating a
	 * new one: preferring -t if it already points at a window, or
	 * otherwise -n if one exists with that name. If neither matches,
	 * fall through and create a window as normal.
	 */
	name = masil_launch == NULL ? args_get(args, 'n') :
	    masil_launch_context_name(masil_launch);
	if (name != NULL) {
		if (masil_launch == NULL)
			expanded = format_single(item, name, c, s, NULL, NULL);
		else
			expanded = xstrdup(name); /* masil: never expand launch names. */
		if (!check_name(expanded)) {
			if (masil_launch != NULL)
				masil_launch_context_set_error(masil_launch,
				    "invalid window name: %s", expanded);
			cmdq_error(item, "invalid window name: %s", expanded);
			free(expanded);
			return (CMD_RETURN_ERROR);
		}
		wname = clean_name(expanded, 0);
		free(expanded);
	}
	if (args_has(args, 'S')) {
		if (idx != -1)
			new_wl = winlink_find_by_index(&s->windows, idx);
		else if (wname != NULL) {
			expanded = format_single(item, wname, c, s, NULL, NULL);
			RB_FOREACH(wl, winlinks, &s->windows) {
				if (strcmp(wl->window->name, expanded) != 0)
					continue;
				if (new_wl == NULL) {
					new_wl = wl;
					continue;
				}
				cmdq_error(item, "multiple windows named %s",
				    wname);
				free(wname);
				free(expanded);
				return (CMD_RETURN_ERROR);
			}
			free(expanded);
		}
	}

	/* Found an existing window to select instead of creating a new one. */
	if (new_wl != NULL) {
		free(wname);
		if (args_has(args, 'd'))
			return (CMD_RETURN_NORMAL);
		if (session_set_current(s, new_wl) == 0)
			server_redraw_session(s);
		if (c != NULL && c->session != NULL)
			s->curw->window->latest = c;
		recalculate_sizes();
		return (CMD_RETURN_NORMAL);
	}

	before = args_has(args, 'b');
	if (args_has(args, 'a') || before) {
		idx = winlink_shuffle_up(s, wl, before);
		if (idx == -1)
			idx = target->idx;
	}

	sc.item = item;
	sc.s = s;
	sc.tc = tc;

	sc.name = wname;
	if (masil_launch == NULL) {
		args_to_vector(args, &sc.argc, &sc.argv);
		sc.environ = environ_create();
		av = args_first_value(args, 'e');
		while (av != NULL) {
			environ_put(sc.environ, av->string, 0);
			av = args_next_value(av);
		}
	} else {
		/* masil: guarded launch values never pass through command arguments. */
		masil_launch_context_argv(masil_launch, &sc.argc, &sc.argv);
		sc.environ = masil_launch_context_environ(masil_launch);
	}

	sc.idx = idx;
	sc.cwd = masil_launch == NULL ? args_get(args, 'c') :
	    masil_launch_context_cwd(masil_launch);

	sc.flags = 0;
	if (args_has(args, 'E') || (count == 1 && *args_string(args, 0) == '\0'))
		sc.flags |= SPAWN_EMPTY;
	if (args_has(args, 'd'))
		sc.flags |= SPAWN_DETACHED;
	if (args_has(args, 'k'))
		sc.flags |= SPAWN_KILL;
	if (masil_launch != NULL) {
		/* masil: launch's cwd is literal and verified by spawn.c. */
		sc.masil_launch = masil_launch;
		sc.flags |= SPAWN_MASIL_STRICT_CWD;
	}

	if ((new_wl = spawn_window(&sc, &cause)) == NULL) {
		if (masil_launch != NULL)
			masil_launch_context_set_error(masil_launch, "%s", cause);
		cmdq_error(item, "create window failed: %s", cause);
		free(cause);
		goto fail;
	}
	if (masil_launch != NULL)
		masil_launch_context_pane_created(masil_launch,
		    new_wl->window->active);
	if (!args_has(args, 'd') || new_wl == s->curw) {
		cmd_find_from_winlink(current, new_wl, 0);
		server_redraw_session_group(s);
	} else
		server_status_session_group(s);

	if (args_has(args, 'P')) {
		if ((template = args_get(args, 'F')) == NULL)
			template = NEW_WINDOW_TEMPLATE;
		cp = format_single(item, template, tc, s, new_wl,
			new_wl->window->active);
		cmdq_print(item, "%s", cp);
		free(cp);
	}

	cmd_find_from_winlink(&fs, new_wl, 0);
	cmdq_insert_hook(s, item, &fs, "after-new-window");

	if (sc.argv != NULL)
		cmd_free_argv(sc.argc, sc.argv);
	environ_free(sc.environ);
	free(wname);
	return (CMD_RETURN_NORMAL);

fail:
	if (sc.argv != NULL)
		cmd_free_argv(sc.argc, sc.argv);
	environ_free(sc.environ);
	free(wname);
	return (CMD_RETURN_ERROR);
}
