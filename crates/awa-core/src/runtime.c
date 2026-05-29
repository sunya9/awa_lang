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
#include <unistd.h>
#include <fcntl.h>
#include <sys/event.h>   // kqueue（mac/BSD のレディネス通知）
#include <sys/socket.h>
#include <netinet/in.h>
#include <arpa/inet.h>
#include <pthread.h>     // ファイル擬似非同期のスレッドプール

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
// 文字列 → 整数（非数値は 0）。
long long awa_string_to_int(void* strobj) { return atoll((char*)strobj + 16); }
// 文字数（UTF-8 コードポイント数。継続バイト 10xxxxxx を除いて数える＝インタプリタの chars() と一致）。
long long awa_string_length(void* strobj) {
  const unsigned char* p = (const unsigned char*)strobj + 16;
  long long n = 0;
  while (*p) { if ((*p & 0xC0) != 0x80) n++; p++; }
  return n;
}
// UTF-8 バイト数（Content-Length 用）。
long long awa_string_byte_length(void* strobj) { return (long long)strlen((char*)strobj + 16); }
// 文字列 → 浮動小数（非数値は 0.0）。
double awa_string_to_float(void* strobj) { return atof((char*)strobj + 16); }
// double → 最短往復表現の String（Rust の to_string と一致させるため精度を 1..17 で探す）。
void* awa_float_str(double x) {
  char buf[32];
  for (int p = 1; p <= 17; p++) {
    snprintf(buf, sizeof buf, "%.*g", p, x);
    if (strtod(buf, 0) == x) break;
  }
  return awa_make_str(buf, (long)strlen(buf));
}
// i 番目（0 始まり）のコードポイントを返す。範囲外は -1。
long long awa_string_char_at(void* strobj, long long idx) {
  if (idx < 0) return -1;
  const unsigned char* p = (const unsigned char*)strobj + 16;
  long long i = 0;
  while (*p) {
    // このバイトが文字の先頭か（継続バイトでない）。
    unsigned char c = *p;
    long long cp;
    int len;
    if (c < 0x80) { cp = c; len = 1; }
    else if ((c & 0xE0) == 0xC0) { cp = c & 0x1F; len = 2; }
    else if ((c & 0xF0) == 0xE0) { cp = c & 0x0F; len = 3; }
    else if ((c & 0xF8) == 0xF0) { cp = c & 0x07; len = 4; }
    else { p++; continue; } // 不正な先頭バイトは飛ばす
    for (int k = 1; k < len; k++) {
      if ((p[k] & 0xC0) != 0x80) { len = k; break; } // 途切れ
      cp = (cp << 6) | (p[k] & 0x3F);
    }
    if (i == idx) return cp;
    i++;
    p += len;
  }
  return -1;
}
// コードポイント c を UTF-8 にエンコードした 1 文字の String。
void* awa_char_to_string(long long c) {
  unsigned char buf[4];
  long n = 0;
  if (c < 0) c = 0;
  if (c < 0x80) { buf[n++] = (unsigned char)c; }
  else if (c < 0x800) {
    buf[n++] = 0xC0 | (c >> 6);
    buf[n++] = 0x80 | (c & 0x3F);
  } else if (c < 0x10000) {
    buf[n++] = 0xE0 | (c >> 12);
    buf[n++] = 0x80 | ((c >> 6) & 0x3F);
    buf[n++] = 0x80 | (c & 0x3F);
  } else {
    buf[n++] = 0xF0 | (c >> 18);
    buf[n++] = 0x80 | ((c >> 12) & 0x3F);
    buf[n++] = 0x80 | ((c >> 6) & 0x3F);
    buf[n++] = 0x80 | (c & 0x3F);
  }
  return awa_make_str((const char*)buf, n);
}

// ===== 非同期イベントループ =====
typedef long long Word;
typedef Word (*ContFn)(void* state, Word x);

// awa の参照カウント解放（codegen が .ll に定義）。継続クロージャの env を解放するのに使う。
extern void awa_release(void* obj);
// 継続の state（codegen が作った {fn, env} の 16 バイト）を後始末する。
// owning convention 上、env の所有権はランタイムへ移っているので、発火後に release する。
static void release_cont(void* state) {
  void* env = *(void**)((char*)state + 8);
  awa_release(env); // env が null（捕獲なし）なら awa_release 側で無視される
  free(state);
}
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
    else if (w->kind == 1) { long h = w->fn(w->state, v); release_cont(w->state); link_fut(h, w->g); }
    else { Word y = w->fn(w->state, v); release_cont(w->state); resolve(w->g, y); }
    free(w);
    w = next;
  }
}
long awa_future_flatmap(long f, ContFn fn, void* state) {
  long g = new_future(0, 0);
  if (futs[f].resolved) { long h = fn(state, futs[f].value); release_cont(state); link_fut(h, g); }
  else { Waiter* w = (Waiter*)malloc(sizeof(Waiter)); w->kind = 1; w->g = g; w->fn = fn; w->state = state; w->next = futs[f].waiters; futs[f].waiters = w; }
  return g;
}
long awa_future_map(long f, ContFn fn, void* state) {
  long g = new_future(0, 0);
  if (futs[f].resolved) { Word y = fn(state, futs[f].value); release_cont(state); resolve(g, y); }
  else { Waiter* w = (Waiter*)malloc(sizeof(Waiter)); w->kind = 2; w->g = g; w->fn = fn; w->state = state; w->next = futs[f].waiters; futs[f].waiters = w; }
  return g;
}
// ===== OS 連携: ソケット非同期（kqueue でレディネスを待つ）=====
// Socket は fd（i64）。accept/read は「読めるようになったら」kqueue が通知し、
// その時に accept()/read() を実行して Future を解決する。write は小データ前提で同期書き込み。
static int kq = -1;
static void ensure_kq(void) { if (kq < 0) kq = kqueue(); }
static void set_nonblock(int fd) { int fl = fcntl(fd, F_GETFL, 0); fcntl(fd, F_SETFL, fl | O_NONBLOCK); }

enum { ACT_ACCEPT = 1, ACT_READ = 2, ACT_CONNECT = 3 };
typedef struct { int fd; int action; long future; int active; } FdW;
static FdW* fdws = 0;
static long fdws_cap = 0, fdws_len = 0;

// fd に対する非同期待ちを登録し、解決待ちの Future を返す。
static long add_fdw(int fd, int action) {
  ensure_kq();
  long g = new_future(0, 0);
  if (fdws_len == fdws_cap) { fdws_cap = fdws_cap ? fdws_cap * 2 : 16; fdws = (FdW*)realloc(fdws, fdws_cap * sizeof(FdW)); }
  fdws[fdws_len].fd = fd; fdws[fdws_len].action = action; fdws[fdws_len].future = g; fdws[fdws_len].active = 1; fdws_len++;
  int filter = (action == ACT_CONNECT) ? EVFILT_WRITE : EVFILT_READ;
  struct kevent ch;
  EV_SET(&ch, fd, filter, EV_ADD | EV_ONESHOT, 0, 0, 0);
  kevent(kq, &ch, 1, NULL, 0, NULL);
  return g;
}

long awa_tcp_listen(Word port) {
  int s = socket(AF_INET, SOCK_STREAM, 0);
  int one = 1; setsockopt(s, SOL_SOCKET, SO_REUSEADDR, &one, sizeof one);
  struct sockaddr_in a; memset(&a, 0, sizeof a);
  a.sin_family = AF_INET; a.sin_addr.s_addr = htonl(INADDR_LOOPBACK); a.sin_port = htons((unsigned short)port);
  if (bind(s, (struct sockaddr*)&a, sizeof a) < 0) { perror("bind"); exit(74); }
  if (listen(s, 16) < 0) { perror("listen"); exit(74); }
  set_nonblock(s);
  return s; // Socket = fd
}
long awa_sock_port(Word fd) {
  struct sockaddr_in a; socklen_t al = sizeof a;
  getsockname((int)fd, (struct sockaddr*)&a, &al);
  return ntohs(a.sin_port);
}
long awa_accept(Word fd) { return add_fdw((int)fd, ACT_ACCEPT); }
long awa_sock_read(Word fd) { return add_fdw((int)fd, ACT_READ); }
long awa_sock_write(Word fd, Word strobj) {
  const char* b = (char*)strobj + 16;
  long n = (long)strlen(b), off = 0;
  while (off < n) { long w = write((int)fd, b + off, n - off); if (w <= 0) break; off += w; }
  return new_future(1, 0); // Future(Nil)
}
void awa_sock_close(Word fd) { close((int)fd); }
long awa_tcp_connect(Word hostobj, Word port) {
  int s = socket(AF_INET, SOCK_STREAM, 0);
  set_nonblock(s);
  struct sockaddr_in a; memset(&a, 0, sizeof a);
  a.sin_family = AF_INET; a.sin_port = htons((unsigned short)port);
  inet_pton(AF_INET, (char*)hostobj + 16, &a.sin_addr);
  connect(s, (struct sockaddr*)&a, sizeof a); // 非ブロッキング → EINPROGRESS
  return add_fdw(s, ACT_CONNECT);
}

// fd が ready になった時の処理（accept/read/connect を実行して Future を解決）。
static void perform_fdw(long idx) {
  FdW* w = &fdws[idx];
  w->active = 0;
  if (w->action == ACT_ACCEPT) {
    int c = accept(w->fd, NULL, NULL);
    if (c >= 0) set_nonblock(c);
    resolve(w->future, (Word)c);
  } else if (w->action == ACT_READ) {
    char buf[65536];
    long n = read(w->fd, buf, sizeof buf);
    if (n < 0) n = 0;
    void* o = awa_make_str(buf, n);
    resolve(w->future, (Word)o);
  } else { // ACT_CONNECT
    resolve(w->future, (Word)w->fd);
  }
}

// ===== OS 連携: ファイル擬似非同期（スレッドプール）=====
// 通常ファイルは kqueue で非同期化できない（常に ready）ので、Node/libuv 同様
// ワーカースレッドでブロッキング read し、完了を EVFILT_USER でメインループへ通知する。
// awa 言語からはスレッドは見えない（ワーカーは生バイトを返すだけ。awa String 生成と
// Future の解決はメインスレッドのみ＝awa_live の競合や継続の並行実行を避ける）。
#define USER_IDENT 1 // EVFILT_USER の識別子（メインループを起こす）

typedef struct Job {
  long future;
  char* path;        // 複製したパス（ワーカーが読む）
  char* data;        // ワーカーが読んだ生バイト（メインが String 化）
  long len;
  struct Job* next;
} Job;

// 投入キュー（メイン→ワーカー）と完了キュー（ワーカー→メイン）。
static Job* job_head = 0; static Job* job_tail = 0;
static pthread_mutex_t job_mu = PTHREAD_MUTEX_INITIALIZER;
static pthread_cond_t job_cv = PTHREAD_COND_INITIALIZER;
static Job* done_head = 0; static Job* done_tail = 0;
static pthread_mutex_t done_mu = PTHREAD_MUTEX_INITIALIZER;
static int pool_started = 0;
static long pending_jobs = 0; // 未完了ジョブ数（メインスレッドのみが触る）

static void* worker_main(void* arg) {
  (void)arg;
  for (;;) {
    pthread_mutex_lock(&job_mu);
    while (!job_head) pthread_cond_wait(&job_cv, &job_mu);
    Job* j = job_head; job_head = j->next; if (!job_head) job_tail = 0;
    pthread_mutex_unlock(&job_mu);
    // ブロッキングなファイル読み込み（ワーカースレッド上）。生バイトだけ持ち帰る。
    FILE* f = fopen(j->path, "rb");
    if (!f) { j->data = (char*)malloc(1); j->len = 0; }
    else {
      fseek(f, 0, SEEK_END); long n = ftell(f); fseek(f, 0, SEEK_SET);
      char* b = (char*)malloc(n > 0 ? n : 1);
      j->len = (long)fread(b, 1, n, f); j->data = b; fclose(f);
    }
    free(j->path); j->path = 0;
    // 完了キューへ積み、メインループを EVFILT_USER で起こす。
    pthread_mutex_lock(&done_mu);
    j->next = 0; if (done_tail) done_tail->next = j; else done_head = j; done_tail = j;
    pthread_mutex_unlock(&done_mu);
    struct kevent ch; EV_SET(&ch, USER_IDENT, EVFILT_USER, 0, NOTE_TRIGGER, 0, 0);
    kevent(kq, &ch, 1, NULL, 0, NULL);
  }
  return 0;
}

static void ensure_pool(void) {
  if (pool_started) return;
  pool_started = 1;
  ensure_kq();
  struct kevent ch; EV_SET(&ch, USER_IDENT, EVFILT_USER, EV_ADD | EV_CLEAR, 0, 0, 0);
  kevent(kq, &ch, 1, NULL, 0, NULL);
  for (int i = 0; i < 4; i++) { pthread_t t; pthread_create(&t, 0, worker_main, 0); pthread_detach(t); }
}

long awa_read_file_async(void* pathobj) {
  ensure_pool();
  long g = new_future(0, 0);
  Job* j = (Job*)malloc(sizeof(Job));
  j->future = g; j->path = strdup((char*)pathobj + 16); j->data = 0; j->len = 0; j->next = 0;
  pending_jobs++;
  pthread_mutex_lock(&job_mu);
  if (job_tail) job_tail->next = j; else job_head = j; job_tail = j;
  pthread_cond_signal(&job_cv);
  pthread_mutex_unlock(&job_mu);
  return g;
}

// 完了したファイルジョブを取り出し String 化して Future を解決（メインスレッドのみ）。
static void drain_done(void) {
  for (;;) {
    pthread_mutex_lock(&done_mu);
    Job* j = done_head;
    if (j) { done_head = j->next; if (!done_head) done_tail = 0; }
    pthread_mutex_unlock(&done_mu);
    if (!j) break;
    void* o = awa_make_str(j->data, j->len);
    free(j->data);
    pending_jobs--;
    long fut = j->future;
    free(j);
    resolve(fut, (Word)o);
  }
}

Word awa_future_run(long f) {
  ensure_kq();
  while (!futs[f].resolved) {
    // 最早タイマー。
    int have_timer = timers_len > 0;
    long mi = 0;
    if (have_timer)
      for (long i = 1; i < timers_len; i++)
        if (timers[i].tick < timers[mi].tick || (timers[i].tick == timers[mi].tick && timers[i].seq < timers[mi].seq)) mi = i;
    int have_fd = 0;
    for (long i = 0; i < fdws_len; i++) if (fdws[i].active) { have_fd = 1; break; }
    if (!have_timer && !have_fd && pending_jobs == 0) exit(70); // deadlock
    // kqueue で「タイマー締切まで」または「fd が ready / ファイルジョブ完了」まで待つ。
    struct timespec ts, *tsp = NULL;
    if (have_timer) {
      long wait = timers[mi].tick - now_ms();
      if (wait < 0) wait = 0;
      ts.tv_sec = wait / 1000; ts.tv_nsec = (wait % 1000) * 1000000; tsp = &ts;
    }
    struct kevent ev;
    int n = kevent(kq, NULL, 0, &ev, 1, tsp);
    if (n > 0) {
      if (ev.filter == EVFILT_USER) { // ワーカーがファイル読み込みを完了
        drain_done();
      } else {
        int fd = (int)ev.ident;
        for (long i = 0; i < fdws_len; i++)
          if (fdws[i].active && fdws[i].fd == fd) { perform_fdw(i); break; }
      }
    } else if (have_timer) { // タイムアウト → 締切のタイマーを発火
      Timer t = timers[mi];
      timers[mi] = timers[timers_len - 1];
      timers_len--;
      resolve(t.id, t.value);
    }
  }
  return futs[f].value;
}
