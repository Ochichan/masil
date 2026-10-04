/*
 * masil UI layer: saved choices and the built-in layer, loaded before the user
 * configuration files when masil is started without -f.
 */

#include <sys/types.h>
#include <sys/stat.h>

#include <limits.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>
#ifdef __APPLE__
#include <libproc.h>
#endif

#include "tmux.h"

static const char masil_ui_layer[] =
#include "masil-ui-layer.h"
;

/* Cleared by -f; the layer and saved choices are then not loaded. */
int	masil_ui_enabled = 1;

/* masil: only executable regular files are exposed as the layer agent. */
static int
masil_ui_agent_exists(const char *path)
{
	struct stat	 sb;

	return (stat(path, &sb) == 0 && S_ISREG(sb.st_mode) &&
	    access(path, X_OK) == 0);
}

/* Find masil-agent beside the running executable. */
static char *
masil_ui_agent_path(void)
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
	xasprintf(&path, "%s/masil-agent", exe);
	if (!masil_ui_agent_exists(path)) {
		free(path);
		return (NULL);
	}
	return (path);
}

/* masil: a PATH-discovered agent is recorded as an absolute executable path. */
static char *
masil_ui_agent_path_from_path(void)
{
	const char	*env;
	char		*copy, *entry, *next, *path;
	char		 resolved[PATH_MAX];

	env = getenv("PATH");
	if (env == NULL)
		return (NULL);
	copy = next = xstrdup(env);
	while ((entry = strsep(&next, ":")) != NULL) {
		if (*entry == '\0')
			entry = ".";
		xasprintf(&path, "%s/masil-agent", entry);
		if (masil_ui_agent_exists(path) &&
		    realpath(path, resolved) != NULL) {
			free(path);
			free(copy);
			return (xstrdup(resolved));
		}
		free(path);
	}
	free(copy);
	return (NULL);
}

/*
 * Saved choices: $XDG_CONFIG_HOME/masil/settings.conf when XDG_CONFIG_HOME is
 * absolute, else ~/.config/masil/settings.conf. masil-agent uses the same rule.
 */
static char *
masil_ui_settings_path(void)
{
	const char	*xdg, *home;
	char		*path;

	xdg = getenv("XDG_CONFIG_HOME");
	if (xdg != NULL && *xdg == '/')
		xasprintf(&path, "%s/masil/settings.conf", xdg);
	else if ((home = find_home()) != NULL)
		xasprintf(&path, "%s/.config/masil/settings.conf", home);
	else
		return (NULL);
	return (path);
}

/* Queue the saved choices and the layer ahead of the configuration files. */
void
masil_ui_load(struct client *c, int flags)
{
	char	*agent, *settings;

	if (!masil_ui_enabled)
		return;

	agent = masil_ui_agent_path();
	if (agent == NULL)
		agent = masil_ui_agent_path_from_path();
	/* masil: a missing agent is explicit rather than shell-resolved later. */
	options_set_string(global_s_options, "@masil-agent", 0, "%s",
	    agent != NULL ? agent : "");
	free(agent);

	settings = masil_ui_settings_path();
	if (settings != NULL) {
		load_cfg(settings, c, NULL, NULL, flags|CMD_PARSE_QUIET, NULL);
		free(settings);
	}
	load_cfg_from_buffer(masil_ui_layer, strlen(masil_ui_layer),
	    "masil-ui-layer", c, NULL, NULL, flags, NULL);
}
