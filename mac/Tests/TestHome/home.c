// A home of its own for every test run.
//
// Everything the app writes lives under one root, and LEGION_CONTROL_HOME moves it (AppPaths.swift).
// A test that reached a store through its default location wrote into the real one instead: the
// bootstrap tests left their refused archives in ~/Library/Application Support/Legion Control, and
// remembered settings landed in the test runner's own preferences. Each test passing a directory of
// its own is only as good as the next test that forgets to.
//
// So the variable is set here, in a constructor that runs when the test bundle is loaded, before
// Swift Testing has started anything: a fresh directory under TMPDIR, removed again when the run
// exits. One a person set is replaced as well, because the run's own is never less isolated.
// LEGION_CONTROL_CONFIG goes, because it names the config file even under a home. IsolationTests
// fails if any of this stops holding.

#include <ftw.h>
#include <limits.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>

static char home[PATH_MAX];

static int remove_entry(const char *path, const struct stat *info, int flag, struct FTW *walk) {
    (void)info;
    (void)flag;
    (void)walk;
    return remove(path);
}

static void remove_home(void) {
    nftw(home, remove_entry, 16, FTW_DEPTH | FTW_PHYS);
}

__attribute__((constructor))
static void legion_control_test_home(void) {
    const char *base = getenv("TMPDIR");
    if (base == NULL || *base == '\0') base = "/tmp";
    size_t length = strlen(base);
    const char *separator = base[length - 1] == '/' ? "" : "/";
    int written = snprintf(home, sizeof home, "%s%slegion-control-tests-XXXXXX", base, separator);
    // Better no test run at all than one that falls back to the real home.
    if (written < 0 || (size_t)written >= sizeof home || mkdtemp(home) == NULL) {
        perror("TestHome: no directory for the run's own LEGION_CONTROL_HOME");
        exit(EXIT_FAILURE);
    }
    setenv("LEGION_CONTROL_HOME", home, 1);
    unsetenv("LEGION_CONTROL_CONFIG");
    atexit(remove_home);
}
