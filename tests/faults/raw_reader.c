/* A provider-named raw terminal reader for core input-commit fixtures.
 *
 * It keeps its executable name (`codex` in the test fixture), so foreground
 * process detection remains verified while its output mirrors `cat -uv` for
 * the bytes core queued into the pane.
 */
#include <errno.h>
#include <termios.h>
#include <unistd.h>

static int
write_all(const char *data, size_t length)
{
	size_t offset = 0;

	while (offset < length) {
		ssize_t written = write(STDOUT_FILENO, data + offset, length - offset);
		if (written > 0) {
			offset += (size_t)written;
			continue;
		}
		if (written == -1 && errno == EINTR)
			continue;
		return -1;
	}
	return 0;
}

static int
show_byte(unsigned char byte)
{
	char shown[2];

	if (byte == '\n')
		return write_all("\n", 1);
	if (byte == 0x7f) {
		shown[0] = '^';
		shown[1] = '?';
		return write_all(shown, sizeof shown);
	}
	if (byte < 0x20) {
		shown[0] = '^';
		shown[1] = (char)(byte ^ 0x40);
		return write_all(shown, sizeof shown);
	}
	shown[0] = (char)byte;
	return write_all(shown, 1);
}

int
main(void)
{
	struct termios settings;
	unsigned char input[4096];

	if (tcgetattr(STDIN_FILENO, &settings) != 0)
		return 1;
	settings.c_iflag = 0;
	settings.c_oflag = OPOST | ONLCR;
	settings.c_lflag &= (tcflag_t)~(ECHO | ICANON | IEXTEN | ISIG);
	settings.c_cflag |= CS8;
	settings.c_cc[VMIN] = 1;
	settings.c_cc[VTIME] = 0;
	if (tcsetattr(STDIN_FILENO, TCSANOW, &settings) != 0)
		return 1;
	if (write_all("\033[?2004h", sizeof "\033[?2004h" - 1) != 0)
		return 1;
	for (;;) {
		ssize_t count = read(STDIN_FILENO, input, sizeof input);
		if (count == 0)
			return 0;
		if (count < 0) {
			if (errno == EINTR)
				continue;
			return 1;
		}
		for (ssize_t index = 0; index < count; index++) {
			if (show_byte(input[index]) != 0)
				return 1;
		}
	}
}
