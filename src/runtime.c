// ===== awa ネイティブランタイム（C）=====
// `awa build` 時に clang でこの .c を .ll と一緒にコンパイル・リンクする
// （codegen.rs が include_str! で取り込み ASYNC_RUNTIME_C として埋め込む）。
//
// 内容:
//   - 同期 I/O（print/read_line/read_file/write_file/now）と awa String 生成
//   - 非同期イベントループ（論理→実時間タイマー、型消去された i64 ワードのペイロード、
//     継続は (state, word)->word の関数ポインタ＝codegen 生成のトランポリン）
// インタプリタ版（eval.rs）と同じ意味モデルを C で実装したもの。
#include <stdlib.h>
#include <stdio.h>
#include <string.h>
#include <time.h>

static long now_ms(void) {
  struct timespec ts;
  clock_gettime(CLOCK_MONOTONIC, &ts);
  return (long)ts.tv_sec * 1000 + ts.tv_nsec / 1000000;
}
static void sleep_ms(long ms) {
  if (ms <= 0) return;
  struct timespec ts;
  ts.tv_sec = ms / 1000;
  ts.tv_nsec = (ms % 1000) * 1000000;
  nanosleep(&ts, 0);
}

// ===== OS 連携: 同期 I/O（awa の String オブジェクトを生成/消費）=====
// awa の String レイアウト: [i64 rc][ptr drop][bytes...]、本体は obj+16。
extern long long awa_live; // 生存ヒープ数。codegen の @awa_live と共有（RC 整合）
static void* awa_make_str(const char* s, long n) {
  char* o = (char*)malloc(16 + n + 1);
  *(long long*)o = 1;       // rc = 1
  *(void**)(o + 8) = 0;     // drop = null（子なし）
  memcpy(o + 16, s, n);
  o[16 + n] = 0;
  awa_live++;               // RC: 生成で +1（awa_release が解放時に -1）
  return o;
}
void awa_print(void* strobj) { printf("%s\n", (char*)strobj + 16); }
void* awa_read_line(void) {
  char buf[8192];
  if (!fgets(buf, sizeof buf, stdin)) buf[0] = 0;
  long n = strlen(buf);
  while (n && (buf[n - 1] == '\n' || buf[n - 1] == '\r')) buf[--n] = 0;
  return awa_make_str(buf, n);
}
void* awa_read_file(void* pathobj) {
  FILE* f = fopen((char*)pathobj + 16, "rb");
  if (!f) { fprintf(stderr, "io error: cannot read %s\n", (char*)pathobj + 16); exit(74); }
  fseek(f, 0, SEEK_END);
  long n = ftell(f);
  fseek(f, 0, SEEK_SET);
  char* b = (char*)malloc(n > 0 ? n : 1);
  long got = (long)fread(b, 1, n, f);
  fclose(f);
  void* o = awa_make_str(b, got);
  free(b);
  return o;
}
void awa_write_file(void* pathobj, void* contentobj) {
  FILE* f = fopen((char*)pathobj + 16, "wb");
  if (!f) { fprintf(stderr, "io error: cannot write %s\n", (char*)pathobj + 16); exit(74); }
  const char* c = (char*)contentobj + 16;
  fwrite(c, 1, strlen(c), f);
  fclose(f);
}
long long awa_now(void) {
  struct timespec ts;
  clock_gettime(CLOCK_REALTIME, &ts);
  return (long long)ts.tv_sec * 1000 + ts.tv_nsec / 1000000;
}

// ===== 非同期イベントループ =====
typedef long long Word;
typedef Word (*ContFn)(void* state, Word x);
typedef struct Waiter { int kind; long g; ContFn fn; void* state; struct Waiter* next; } Waiter;
typedef struct { int resolved; Word value; Waiter* waiters; } Future;
static Future* futs = 0;
static long futs_cap = 0, futs_len = 0;
typedef struct { long tick; long seq; long id; Word value; } Timer;
static Timer* timers = 0;
static long timers_cap = 0, timers_len = 0;
static long seq_ctr = 0;

static long new_future(int resolved, Word v) {
  if (futs_len == futs_cap) { futs_cap = futs_cap ? futs_cap * 2 : 16; futs = (Future*)realloc(futs, futs_cap * sizeof(Future)); }
  long id = futs_len++;
  futs[id].resolved = resolved;
  futs[id].value = v;
  futs[id].waiters = 0;
  return id;
}
long awa_future_pure(Word v) { return new_future(1, v); }
long awa_future_delay(Word ms, Word v) {
  long id = new_future(0, 0);
  if (timers_len == timers_cap) { timers_cap = timers_cap ? timers_cap * 2 : 16; timers = (Timer*)realloc(timers, timers_cap * sizeof(Timer)); }
  timers[timers_len].tick = now_ms() + ms;
  timers[timers_len].seq = seq_ctr++;
  timers[timers_len].id = id;
  timers[timers_len].value = v;
  timers_len++;
  return id;
}
static void resolve(long id, Word v);
static void link_fut(long h, long g) {
  if (futs[h].resolved) resolve(g, futs[h].value);
  else { Waiter* w = (Waiter*)malloc(sizeof(Waiter)); w->kind = 0; w->g = g; w->fn = 0; w->state = 0; w->next = futs[h].waiters; futs[h].waiters = w; }
}
static void resolve(long id, Word v) {
  futs[id].resolved = 1;
  futs[id].value = v;
  Waiter* w = futs[id].waiters;
  futs[id].waiters = 0;
  while (w) {
    Waiter* next = w->next;
    if (w->kind == 0) resolve(w->g, v);
    else if (w->kind == 1) { long h = w->fn(w->state, v); link_fut(h, w->g); }
    else { Word y = w->fn(w->state, v); resolve(w->g, y); }
    free(w);
    w = next;
  }
}
long awa_future_flatmap(long f, ContFn fn, void* state) {
  long g = new_future(0, 0);
  if (futs[f].resolved) { long h = fn(state, futs[f].value); link_fut(h, g); }
  else { Waiter* w = (Waiter*)malloc(sizeof(Waiter)); w->kind = 1; w->g = g; w->fn = fn; w->state = state; w->next = futs[f].waiters; futs[f].waiters = w; }
  return g;
}
long awa_future_map(long f, ContFn fn, void* state) {
  long g = new_future(0, 0);
  if (futs[f].resolved) { Word y = fn(state, futs[f].value); resolve(g, y); }
  else { Waiter* w = (Waiter*)malloc(sizeof(Waiter)); w->kind = 2; w->g = g; w->fn = fn; w->state = state; w->next = futs[f].waiters; futs[f].waiters = w; }
  return g;
}
Word awa_future_run(long f) {
  while (!futs[f].resolved) {
    if (timers_len == 0) exit(70); // deadlock
    long mi = 0;
    for (long i = 1; i < timers_len; i++)
      if (timers[i].tick < timers[mi].tick || (timers[i].tick == timers[mi].tick && timers[i].seq < timers[mi].seq)) mi = i;
    Timer t = timers[mi];
    timers[mi] = timers[timers_len - 1];
    timers_len--;
    sleep_ms(t.tick - now_ms()); // 締切まで実時間で待つ
    resolve(t.id, t.value);
  }
  return futs[f].value;
}
