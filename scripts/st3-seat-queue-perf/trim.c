// Diagnostic preload for `startup`: tell freed heap from live heap in a daemon.
//
// Build: cc -O2 -shared -fPIC -o trim.so trim.c
//
// Inside st3 only, SIGRTMIN+3 writes malloc_info to $PERF_TRIM_TRIGGER.before,
// calls malloc_trim(0), writes malloc_info to .after and malloc_trim's result
// to .done. It starts no thread, because a thread moved the heap layout under
// test. malloc_info is not async-signal-safe, so signal only a quiet daemon.
#define _GNU_SOURCE
#include <malloc.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

extern char *program_invocation_short_name;
static char trigger[4096];

static void dump(const char *suffix) {
  char path[4200];
  snprintf(path, sizeof path, "%s.%s", trigger, suffix);
  FILE *out = fopen(path, "w");
  if (!out) return;
  malloc_info(0, out);
  fclose(out);
}

static void handle(int signal) {
  (void)signal;
  dump("before");
  int released = malloc_trim(0);
  dump("after");
  char path[4200];
  snprintf(path, sizeof path, "%s.done", trigger);
  FILE *done = fopen(path, "w");
  if (done) {
    fprintf(done, "%d\n", released);
    fclose(done);
  }
}

__attribute__((constructor)) static void start(void) {
  // setsid and nice exec st3 with this preload; only st3 installs the handler.
  if (strcmp(program_invocation_short_name, "st3") != 0) return;
  const char *value = getenv("PERF_TRIM_TRIGGER");
  if (!value || strlen(value) >= sizeof trigger) return;
  strcpy(trigger, value);
  unsetenv("LD_PRELOAD");
  unsetenv("PERF_TRIM_TRIGGER");
  struct sigaction action;
  memset(&action, 0, sizeof action);
  action.sa_handler = handle;
  sigaction(SIGRTMIN + 3, &action, NULL);
}
