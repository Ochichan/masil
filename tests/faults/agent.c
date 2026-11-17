/* A deterministic native TUI stand-in. Never invokes a model or reads credentials.
 *
 * Lines typed into it change its state, except these test commands, which
 * run `masil-agent` as a provider hook would, from a request directory DIR
 * (args and env as NUL-separated lists, stdin as a file) and write out, err,
 * status and finally done into DIR:
 *   hook DIR                  runs the hook as this process's child
 *   hook-async DIR            the same without waiting, so that hooks of one
 *                             provider can run at the same time
 *   hook-detached DIR         through a relay in its own session, like a
 *                             shared provider daemon
 *   hook-nested DIR OTHER     through the program OTHER started by a shell,
 *                             like a nested provider
 * `PROGRAM --relay DIR` runs one hook and exits. */
#include <sys/types.h>
#include <sys/wait.h>

#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>

#define LIST_BYTES 65536
#define LIST_ITEMS 64

static size_t read_list(const char *dir, const char *name, char *buffer, char **items)
{
    char path[4096];
    snprintf(path, sizeof path, "%s/%s", dir, name);
    FILE *file = fopen(path, "rb");
    if (file == NULL)
        return 0;
    size_t length = fread(buffer, 1, LIST_BYTES - 1, file);
    fclose(file);
    buffer[length] = '\0';
    size_t count = 0;
    for (size_t at = 0; at < length && count < LIST_ITEMS - 1;) {
        items[count++] = buffer + at;
        at += strlen(buffer + at) + 1;
    }
    items[count] = NULL;
    return count;
}

static void write_text(const char *dir, const char *name, const char *text)
{
    char path[4096];
    snprintf(path, sizeof path, "%s/%s", dir, name);
    FILE *file = fopen(path, "w");
    if (file != NULL) {
        fputs(text, file);
        fclose(file);
    }
}

static int run_hook(const char *dir)
{
    static char args_buffer[LIST_BYTES], env_buffer[LIST_BYTES];
    char *args[LIST_ITEMS], *env[LIST_ITEMS];
    if (read_list(dir, "args", args_buffer, args) == 0)
        return 2;
    read_list(dir, "env", env_buffer, env);
    pid_t child = fork();
    if (child == 0) {
        char path[4096];
        for (size_t i = 0; env[i] != NULL; i++)
            putenv(env[i]);
        snprintf(path, sizeof path, "%s/stdin", dir);
        int in = open(path, O_RDONLY);
        snprintf(path, sizeof path, "%s/out", dir);
        int out = open(path, O_WRONLY | O_CREAT | O_TRUNC, 0600);
        snprintf(path, sizeof path, "%s/err", dir);
        int err = open(path, O_WRONLY | O_CREAT | O_TRUNC, 0600);
        if (in < 0 || out < 0 || err < 0)
            _exit(126);
        dup2(in, 0);
        dup2(out, 1);
        dup2(err, 2);
        execv(args[0], args);
        _exit(127);
    }
    int status = 0;
    char text[32];
    if (child < 0 || waitpid(child, &status, 0) < 0)
        snprintf(text, sizeof text, "-1");
    else
        snprintf(text, sizeof text, "%d", WIFEXITED(status) ? WEXITSTATUS(status) : -1);
    write_text(dir, "status", text);
    write_text(dir, "done", "1");
    return 0;
}

static void relay(char *const program, const char *dir, int detached)
{
    pid_t child = fork();
    if (child != 0)
        return;
    if (detached)
        setsid();
    execlp(program, program, "--relay", dir, (char *)NULL);
    _exit(127);
}

static void nested(const char *other, const char *dir)
{
    if (fork() != 0)
        return;
    execl("/bin/sh", "sh", "-c", "\"$0\" --relay \"$1\"", other, dir, (char *)NULL);
    _exit(127);
}

int main(int argc, char **argv)
{
    char line[4096];
    unsigned int inputs = 0;
    if (argc == 3 && strcmp(argv[1], "--relay") == 0)
        return run_hook(argv[2]);
    if (argc == 2 && strcmp(argv[1], "--help") == 0) {
        /* Codex 0.156 and later list --no-daemon; tests opt in. */
        printf("usage: %s\n", argv[0]);
        if (getenv("MASIL_TEST_CODEX_DAEMON_FLAG") != NULL)
            printf("      --no-daemon\n");
        return 0;
    }
    setvbuf(stdout, NULL, _IONBF, 0);
    for (int i = 1; i < argc; i++)
        printf("ARG[%d]=%s\n", i, argv[i]);
    printf("\033[2J\033[HSTATE:idle\n");
    for (;;) {
        // A read cut short by a stop and continue (Ctrl-Z, fg) is not the
        // end of input.
        if (fgets(line, sizeof line, stdin) == NULL) {
            if (ferror(stdin) && errno == EINTR) {
                clearerr(stdin);
                continue;
            }
            break;
        }
        line[strcspn(line, "\r\n")] = '\0';
        if (strcmp(line, "exit") == 0)
            return 0;
        if (strcmp(line, "progress") == 0) {
            printf("\033]9;4;3\007");
            continue;
        }
        if (strcmp(line, "progress-reset") == 0) {
            printf("\033]9;4;0;0\007");
            continue;
        }
        if (strncmp(line, "hook ", 5) == 0) {
            run_hook(line + 5);
            continue;
        }
        if (strncmp(line, "hook-async ", 11) == 0) {
            if (fork() == 0)
                _exit(run_hook(line + 11));
            continue;
        }
        if (strncmp(line, "hook-detached ", 14) == 0) {
            relay(argv[0], line + 14, 1);
            continue;
        }
        if (strncmp(line, "hook-nested ", 12) == 0) {
            char *dir = line + 12;
            char *other = strchr(dir, ' ');
            if (other != NULL) {
                *other++ = '\0';
                nested(other, dir);
            }
            continue;
        }
        printf("\033[2J\033[HSTATE:%s\nINPUTS:%u\n", line, ++inputs);
    }
    return 0;
}
