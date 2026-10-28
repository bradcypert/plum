// Observe a resource retained by a guard's closure before function exit.
static long long live;
long long guard_open(void) { return ++live; }
void guard_close(long long h) { (void)h; --live; }
long long guard_live(void) { return live; }
