/*
 * rmux UI layer: saved choices and the built-in layer, loaded before the user
 * configuration files when rmux is started without -f.
 */

#include <sys/types.h>

#include <limits.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>
#ifdef __APPLE__
#include <libproc.h>
#endif

#include "tmux.h"

static const char rmux_ui_layer[] =
#include "rmux-ui-layer.h"
;

/* Cleared by -f; the layer and saved choices are then not loaded. */
int	rmux_ui_enabled = 1;

/* Find rmux-agent beside the running executable. */
static char *
rmux_ui_agent_path(void)
{
	char	 exe[PATH_MAX], *slash, *path;
#if defined(__APPLE__)
	if (proc_pidpath(getpid(), exe, sizeof exe) <= 0)
		return (NULL);
#elif defined(__linux__)
	ssize_t	 n;

	n = readlink("/proc/self/exe", exe, sizeof exe - 1);
	if (n <= 0)
		return (NULL);
	exe[n] = '\0';
#else
	return (NULL);
#endif
	slash = strrchr(exe, '/');
	if (slash == NULL)
		return (NULL);
	*slash = '\0';
	xasprintf(&path, "%s/rmux-agent", exe);
	if (access(path, X_OK) != 0) {
		free(path);
		return (NULL);
	}
	return (path);
}

/*
 * Saved choices: $XDG_CONFIG_HOME/rmux/settings.conf when XDG_CONFIG_HOME is
 * absolute, else ~/.config/rmux/settings.conf. rmux-agent uses the same rule.
 */
static char *
rmux_ui_settings_path(void)
{
	const char	*xdg, *home;
	char		*path;

	xdg = getenv("XDG_CONFIG_HOME");
	if (xdg != NULL && *xdg == '/')
		xasprintf(&path, "%s/rmux/settings.conf", xdg);
	else if ((home = find_home()) != NULL)
		xasprintf(&path, "%s/.config/rmux/settings.conf", home);
	else
		return (NULL);
	return (path);
}

/* Queue the saved choices and the layer ahead of the configuration files. */
void
rmux_ui_load(struct client *c, int flags)
{
	char	*agent, *settings;

	if (!rmux_ui_enabled)
		return;

	agent = rmux_ui_agent_path();
	options_set_string(global_s_options, "@rmux-agent", 0, "%s",
	    agent != NULL ? agent : "rmux-agent");
	free(agent);

	settings = rmux_ui_settings_path();
	if (settings != NULL) {
		load_cfg(settings, c, NULL, NULL, flags|CMD_PARSE_QUIET, NULL);
		free(settings);
	}
	load_cfg_from_buffer(rmux_ui_layer, strlen(rmux_ui_layer),
	    "rmux-ui-layer", c, NULL, NULL, flags, NULL);
}
