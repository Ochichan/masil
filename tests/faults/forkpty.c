/* Test-only macOS interposer. Never linked into or shipped with masil. */
#include <sys/types.h>
#include <errno.h>
#include <stdlib.h>
#include <unistd.h>
#include <util.h>

static pid_t
fail_selected_forkpty(int *master, char *name, struct termios *term,
    struct winsize *size)
{
	const char *marker = getenv("MASIL_TEST_FAIL_FORKPTY_FILE");

	if (marker != NULL && access(marker, F_OK) == 0) {
		errno = EAGAIN;
		return (-1);
	}
	/* dyld leaves calls from the interposing image bound to the original. */
	return (forkpty(master, name, term, size));
}

__attribute__((used, section("__DATA,__interpose")))
static const struct { const void *replacement; const void *original; }
forkpty_interpose = { (const void *)&fail_selected_forkpty, (const void *)&forkpty };
