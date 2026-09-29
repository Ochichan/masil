/* A deterministic native TUI stand-in. Never invokes a model or reads credentials. */
#include <stdio.h>
#include <string.h>

int main(int argc, char **argv)
{
    char line[4096];
    unsigned int inputs = 0;
    setvbuf(stdout, NULL, _IONBF, 0);
    for (int i = 1; i < argc; i++)
        printf("ARG[%d]=%s\n", i, argv[i]);
    printf("\033[2J\033[HSTATE:idle\n");
    while (fgets(line, sizeof line, stdin) != NULL) {
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
        printf("\033[2J\033[HSTATE:%s\nINPUTS:%u\n", line, ++inputs);
    }
    return 0;
}
