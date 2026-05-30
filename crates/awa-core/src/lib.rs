
pub mod ast;
pub mod codegen;
pub mod error;
pub mod eval;
pub mod fmt;
pub mod infer;
pub mod lexer;
pub mod match_check;
pub mod parser;
pub mod query;
pub mod resolve;
pub mod span;
pub mod stdlib;
pub mod token;
pub mod types;
pub mod value;

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use ast::TStmt;
use error::{AwaError, LocatedError, report};
use eval::eval_program;
use infer::Checker;
use parser::Parser;
use types::Type;
use value::{Env, Value};

// CLI / LSP など下流クレートが使う公開再エクスポート。
pub use codegen::ASYNC_RUNTIME_C;
pub use resolve::LoadedModule;

// ===== パイプライン =====
// tokenize → parse → type-check → eval を ? で連結。どこで失敗しても Err が伝わる。
// 型検査（checker.check_program）を eval の前に挟むのが Phase 2 の要。型エラーは
// 実行前に捕まり、eval には well-typed なプログラムだけが進む。
// env / checker をともに借用で受け取るので、ファイル実行（使い捨て）も
// REPL（行をまたいで持ち越す）もこの 1 つの関数で賄える。
// 各文の (値, 型) を順に返す。型検査は型付き AST（TStmt）を返し、eval はそれを実行する。
// 型宣言は値文を生まないので、値と型はどちらも Let/Expr 文だけから同順で取れる。
pub fn run(
    source: &str,
    env: &mut Env,
    checker: &mut Checker,
) -> Result<Vec<(Value, Type)>, LocatedError> {
    let tokens = lexer::tokenize(source)?;
    let mut parser = Parser::new(tokens);
    let program = parser.parse_program()?;
    let typed = checker.check_program(&program)?;
    let values = eval_program(&typed, env)?;
    // 表示用に各 Let/Expr 文の型を取り出す（Type 文は飛ばす）。
    let types = typed.iter().filter_map(|ts| match ts {
        TStmt::Let(_, te) | TStmt::Expr(te) => Some(te.ty.clone()),
        TStmt::Type(_) | TStmt::Impl(_) => None,
    });
    Ok(values.into_iter().zip(types).collect())
}

// 単一ソースを字句解析→構文解析→型検査するだけ（eval/codegen はしない）。
// LSP の診断などで「エラーの有無と位置」を得るのに使う。awa は最初のエラーで止まる。
pub fn check_source(source: &str) -> Result<(), LocatedError> {
    let tokens = lexer::tokenize(source)?;
    let program = Parser::new(tokens).parse_program()?;
    let mut checker = Checker::new();
    checker.check_program(&program)?;
    Ok(())
}

// ソースを型検査し、型付きプログラムを LLVM IR テキストへコンパイルする（単一ソース）。
pub fn compile_to_llvm(source: &str) -> Result<String, LocatedError> {
    let tokens = lexer::tokenize(source)?;
    let program = Parser::new(tokens).parse_program()?;
    let mut checker = Checker::new();
    let typed = checker.check_program(&program)?;
    Ok(codegen::compile_program(&typed)?)
}

// ===== モジュールローダ（Phase 10）=====
// エントリファイルから import を辿って全モジュールを読み込み、依存順（依存先が先）に並べる。
// import パスは root（= 既定でカレントディレクトリ）からの相対 `seg/seg.awa`。
// 循環 import は検出してエラー。各ファイルは自分のソースで構文エラーを報告（位置を保てる）。
// root: プロジェクトのルート（エントリの親）。stdlib: 標準ライブラリのルート（`std/` を含むディレクトリ）。
// `import std/...` は stdlib から、依存名で始まれば依存ルートから、それ以外は root から解決する。
pub fn load_program(
    root: &PathBuf,
    stdlib: Option<&Path>,
    entry_path: &str,
) -> Result<Vec<LoadedModule>, String> {
    load_all(root, stdlib, entry_path, &HashMap::new(), &HashMap::new())
}

// path 依存つきでロードする（awa.toml の [dependencies] name = { path } 用）。
// deps: 依存名 → そのパッケージのソースディレクトリ。`import name/x` を <dir>/x.awa に解決する。
pub fn load_program_with_deps(
    root: &PathBuf,
    stdlib: Option<&Path>,
    entry_path: &str,
    deps: &HashMap<String, PathBuf>,
) -> Result<Vec<LoadedModule>, String> {
    load_all(root, stdlib, entry_path, &HashMap::new(), deps)
}

// 1 モジュールのソースだけを解決して返す（LSP の補完がモジュールのメンバーを引くのに使う）。
// 解決規則は load_module と同じ: std は焼き込み（ディスクがあれば優先）、他は root 相対。
pub fn resolve_module_source(
    root: &Path,
    stdlib: Option<&Path>,
    segs: &[String],
) -> Option<String> {
    let is_std = segs.first().map(|s| s.as_str()) == Some("std");
    let mut path = match (is_std, stdlib) {
        (true, Some(s)) => s.to_path_buf(),
        _ => root.to_path_buf(),
    };
    for s in segs {
        path.push(s);
    }
    path.set_extension("awa");
    match std::fs::read_to_string(&path) {
        Ok(disk) => Some(disk),
        Err(_) if is_std => stdlib::embedded_source(segs).map(str::to_string),
        Err(_) => None,
    }
}

// load_program と同じだが、モジュールキー（"seg/seg"）→ソースの上書きを受け取る。
// LSP がディスク未保存のバッファ内容で検査するために使う（該当モジュールはディスクを読まない）。
pub fn load_program_with_overrides(
    root: &PathBuf,
    stdlib: Option<&Path>,
    entry_path: &str,
    overrides: &HashMap<String, String>,
) -> Result<Vec<LoadedModule>, String> {
    load_all(root, stdlib, entry_path, overrides, &HashMap::new())
}

// ローダの状態と設定。引数が増えすぎたので 1 つにまとめた。
struct LoadCtx<'a> {
    root: &'a Path,
    stdlib: Option<&'a Path>,
    overrides: &'a HashMap<String, String>,
    deps: &'a HashMap<String, PathBuf>,
    loaded: Vec<LoadedModule>,
    done: HashSet<String>,
    in_progress: HashSet<String>,
}

fn load_all(
    root: &Path,
    stdlib: Option<&Path>,
    entry_path: &str,
    overrides: &HashMap<String, String>,
    deps: &HashMap<String, PathBuf>,
) -> Result<Vec<LoadedModule>, String> {
    // エントリのモジュールパス: ".awa" を外して "/" で分割。
    let rel = entry_path.strip_suffix(".awa").unwrap_or(entry_path);
    let entry_segs: Vec<String> = rel.split('/').map(|s| s.to_string()).collect();
    let mut ctx = LoadCtx {
        root,
        stdlib,
        overrides,
        deps,
        loaded: Vec::new(),
        done: HashSet::new(),
        in_progress: HashSet::new(),
    };
    ctx.load(&entry_segs, true)?;
    Ok(ctx.loaded)
}

impl LoadCtx<'_> {
    // segs を解決すべきファイルパスと is_std を返す。
    //  - "std/..."        → stdlib ルート配下（無ければ後で焼き込みにフォールバック）
    //  - "<dep>/rest..."  → 依存ディレクトリ配下 rest.awa（依存名は接頭辞。bare は <dir>/<dep>.awa）
    //  - それ以外          → プロジェクト root 配下
    fn module_path(&self, segs: &[String]) -> (PathBuf, bool) {
        let head = segs.first().map(|s| s.as_str()).unwrap_or("");
        if head == "std" {
            let mut p = self.stdlib.unwrap_or(self.root).to_path_buf();
            for s in segs {
                p.push(s);
            }
            p.set_extension("awa");
            return (p, true);
        }
        if let Some(dir) = self.deps.get(head) {
            let mut p = dir.clone();
            if segs.len() == 1 {
                p.push(head); // bare `import dep` → <dir>/<dep>.awa
            } else {
                for s in &segs[1..] {
                    p.push(s); // 依存名を剥がして残りを dir 配下へ
                }
            }
            p.set_extension("awa");
            return (p, false);
        }
        let mut p = self.root.to_path_buf();
        for s in segs {
            p.push(s);
        }
        p.set_extension("awa");
        (p, false)
    }

    fn load(&mut self, segs: &[String], is_entry: bool) -> Result<(), String> {
        let key = segs.join("/");
        if self.done.contains(&key) {
            return Ok(()); // 既に読み込み済み（共有 import）
        }
        if !self.in_progress.insert(key.clone()) {
            return Err(format!("import が循環しています: {}", key));
        }
        let (path, is_std) = self.module_path(segs);
        // 上書き（LSP のバッファ）が最優先。次に std はバイナリ焼き込み（cwd/環境に非依存。
        // ただし実ファイルがあればそれを優先＝標準ライブラリ開発の上書き）。最後に通常のディスク読み。
        let source = if let Some(buf) = self.overrides.get(&key) {
            buf.clone()
        } else if is_std {
            match std::fs::read_to_string(&path) {
                Ok(disk) => disk,
                Err(disk_err) => stdlib::embedded_source(segs).map(str::to_string).ok_or_else(
                    || format!("標準ライブラリモジュールがありません: {key} ({disk_err})"),
                )?,
            }
        } else {
            std::fs::read_to_string(&path)
                .map_err(|e| format!("モジュールを読めません ({}): {}", path.display(), e))?
        };
        // 構文解析（このファイルのソースでエラー位置を出す）。
        let parsed = (|| {
            let tokens = lexer::tokenize(&source)?;
            Parser::new(tokens).parse_module()
        })()
        .map_err(|e| report(&e, &source))?;
        // 依存先を先に読み込む（post-order で依存順に並ぶ）。
        for imp in &parsed.imports {
            self.load(&imp.path, false)?;
        }
        self.in_progress.remove(&key);
        self.done.insert(key);
        // エントリは接頭辞なし（既存の単一ファイル挙動・@main を維持）、他は "seg.seg." で修飾。
        let prefix = if is_entry {
            String::new()
        } else {
            format!("{}.", segs.join("."))
        };
        self.loaded.push(LoadedModule {
            path: segs.to_vec(),
            prefix,
            imports: parsed.imports,
            exports: parsed.exports,
            stmts: parsed.stmts,
            source,
        });
        Ok(())
    }
}

// ローダ＋名前解決＋型検査を通し、フラットな型付きプログラムを返す。
// エラーは整形済み文字列で返す（単一モジュールなら位置つき、複数なら簡易表示）。
pub fn check_program_modules(modules: &[LoadedModule]) -> Result<Vec<TStmt>, String> {
    let resolved = resolve::resolve(modules).map_err(|e| format_error(&e, modules))?;
    let mut checker = Checker::new();
    let typed = checker
        .check_program(&resolved.stmts)
        .map_err(|e| format_error(&e, modules))?;
    // 型検査後にメソッド可視性を検査（非公開メソッドの越境呼び出しを弾く）。
    resolve::check_visibility(&typed, &resolved).map_err(|e| format_error(&e, modules))?;
    // 入口 main（引数なし関数）があるとき、トップレベルのむき出しの式は禁止。
    // 処理は main の中に書く（script 形式と main 形式の混在を防ぐ）。
    let has_entry_main = typed.iter().any(|s| {
        matches!(s, TStmt::Let(n, te)
            if n == "main"
            && matches!(&te.kind, ast::TExprKind::Lambda(p, _) if p.is_empty()))
    });
    if has_entry_main {
        if let Some(TStmt::Expr(e)) = typed.iter().find(|s| matches!(s, TStmt::Expr(_))) {
            let err = LocatedError::at(AwaError::EntryConflict, e.span);
            return Err(format_error(&err, modules));
        }
    }
    Ok(typed)
}

// LSP 向け: 1 つの開いている文書を、その import を解決した上で型検査する。
// entry_source はバッファ内容（ディスク未保存でも検査できるよう上書きする）。
// 返すエラーの span は「その文書内のローカル offset」なので、そのまま範囲に変換できる。
// import 先（std や兄弟ファイル）で起きたエラーは、この文書には出しようがないので span なし。
pub fn check_document(
    root: &PathBuf,
    stdlib: Option<&Path>,
    entry_path: &str,
    entry_source: &str,
) -> Result<(), LocatedError> {
    let rel = entry_path.strip_suffix(".awa").unwrap_or(entry_path);
    let entry_key = rel.split('/').collect::<Vec<_>>().join("/");
    let mut overrides = HashMap::new();
    overrides.insert(entry_key.clone(), entry_source.to_string());

    let modules = match load_program_with_overrides(root, stdlib, entry_path, &overrides) {
        Ok(m) => m,
        Err(msg) => {
            // ロード失敗の典型は「編集中の文書自身の字句/構文エラー」。バッファを
            // 直接解析して span 付きで返す（ローカル offset なのでそのまま使える）。
            let tokens = lexer::tokenize(entry_source)?;
            Parser::new(tokens).parse_module()?;
            // 文書自身は解析できた＝欠落 import や依存先の問題。span なしで概要を返す。
            return Err(LocatedError {
                error: AwaError::Io(msg),
                span: None,
            });
        }
    };

    // resolve→型検査→可視性。エラーは連結後のグローバル span なので、文書ローカルへ畳む。
    let localize = |e: LocatedError| localize_to_entry(e, &modules);
    let resolved = resolve::resolve(&modules).map_err(localize)?;
    let mut checker = Checker::new();
    let typed = checker.check_program(&resolved.stmts).map_err(localize)?;
    resolve::check_visibility(&typed, &resolved).map_err(localize)?;
    Ok(())
}

// 連結後のグローバル span を、エントリ文書内のローカル offset へ変換する。
// エントリは prefix == "" の唯一のモジュール。span がエントリ範囲外（依存先）なら span を落とす。
fn localize_to_entry(e: LocatedError, modules: &[LoadedModule]) -> LocatedError {
    let Some(span) = e.span else { return e };
    if let Some(i) = modules.iter().position(|m| m.prefix.is_empty()) {
        let base = resolve::module_base(modules, i);
        let len = modules[i].source.chars().count();
        if span.start >= base && span.start <= base + len {
            return LocatedError {
                error: e.error,
                span: Some(span::Span::new(
                    span.start - base,
                    span.end.saturating_sub(base),
                )),
            };
        }
    }
    LocatedError {
        error: e.error,
        span: None,
    }
}

// エラー整形: 連結後の span がどのモジュールの座標かを逆引きし、そのファイルのソース＋
// ローカル位置で報告する（複数モジュールでもファイル名つきで「行:列」とキャレットを出せる）。
pub fn format_error(e: &LocatedError, modules: &[LoadedModule]) -> String {
    let Some(span) = e.span else {
        return format!("error: {}", e.error);
    };
    for (i, m) in modules.iter().enumerate() {
        let base = resolve::module_base(modules, i);
        let len = m.source.chars().count();
        if span.start >= base && span.start <= base + len {
            let local = span::Span::new(span.start - base, span.end.saturating_sub(base));
            // 単一モジュールならファイル名は省略（従来の見た目）、複数ならパスを出す。
            let filename = if modules.len() > 1 {
                Some(m.path.join("/"))
            } else {
                None
            };
            return error::report_at(&e.error, Some(local), &m.source, filename.as_deref());
        }
    }
    format!("error: {}", e.error)
}

// 全モジュールを名前解決・型検査し、LLVM IR テキストへコンパイルする。
pub fn compile_modules_to_ir(modules: &[LoadedModule]) -> Result<String, String> {
    let typed = check_program_modules(modules)?;
    codegen::compile_program(&typed).map_err(|e| format_error(&LocatedError::from(e), modules))
}

// ===== テスト =====
#[cfg(test)]
mod tests {
    use super::*;
    use error::AwaError;

    // テストは従来どおり AwaError（種類）と比較したいので、位置（span）は捨てて射影する。
    fn eval(src: &str) -> Result<Value, AwaError> {
        let mut env = eval::builtins_env();
        let mut checker = Checker::new();
        run(src, &mut env, &mut checker)
            .map(|rows| rows.last().map(|(v, _)| v.clone()).unwrap_or(Value::Unit))
            .map_err(|e| e.error)
    }

    // 値だけを取り出すヘルパ（run は (値, 型) を返すようになったため）。
    fn run_values(
        src: &str,
        env: &mut Env,
        checker: &mut Checker,
    ) -> Result<Vec<Value>, AwaError> {
        run(src, env, checker)
            .map(|rows| rows.into_iter().map(|(v, _)| v).collect())
            .map_err(|e| e.error)
    }

    fn num(n: f64) -> Value {
        Value::Int(n as i64)
    }

    // ===== 非同期: 論理時間イベントループ（Phase 11）=====

    #[test]
    fn async_doで複数のdelayを合成() {
        // do が Future の flatMap 連鎖へ脱糖され、run がイベントループを回して合成する。
        assert_eq!(
            eval("run(do { x <- delay(10, 1); y <- delay(5, 2); pure(x + y) })"),
            Ok(Value::Int(3))
        );
    }

    #[test]
    fn async_pureは即解決() {
        assert_eq!(eval("run(pure(42))"), Ok(Value::Int(42)));
    }

    #[test]
    fn async_mapで変換() {
        assert_eq!(
            eval("run(delay(3, 21).map(fn(n: Int) -> Int { n * 2 }))"),
            Ok(Value::Int(42))
        );
    }

    #[test]
    fn async_複数行のrun_doが書ける() {
        // run( ... ) の括弧の中でも do ブロックの改行が文区切りになる。
        let src = "run(do {\n  x <- delay(1, 10)\n  y <- delay(2, 20)\n  pure(x + y)\n})";
        assert_eq!(eval(src), Ok(Value::Int(30)));
    }

    #[test]
    fn example_async() {
        let src = include_str!("../../../examples/async.awa");
        let mut env = eval::builtins_env();
        let mut checker = Checker::new();
        let vals = run_values(src, &mut env, &mut checker).expect("型検査・評価が通るはず");
        assert_eq!(vals.first(), Some(&Value::Int(3)));
        assert_eq!(vals.last(), Some(&Value::Int(12)));
    }

    // ===== OS 連携: 同期 I/O・時間（Phase 12 / Stage 1）=====

    #[test]
    fn io_ファイル書き込みと読み出し() {
        let id = std::process::id();
        let path = std::env::temp_dir().join(format!("awa_io_{}.txt", id));
        let p = path.to_string_lossy().replace('\\', "/");
        let src = format!("let _ = write_file(\"{p}\", \"hi os\")\nread_file(\"{p}\")");
        let got = eval(&src);
        let _ = std::fs::remove_file(&path);
        assert_eq!(got, Ok(Value::Str("hi os".to_string())));
    }

    #[test]
    fn io_printはunitを返す() {
        assert_eq!(eval("print(\"x\")"), Ok(Value::Unit));
    }

    #[test]
    fn io_string_to_int() {
        assert_eq!(eval("string_to_int(\"42\")"), Ok(Value::Int(42)));
        assert_eq!(eval("string_to_int(\"-7\")"), Ok(Value::Int(-7)));
        assert_eq!(eval("string_to_int(\"abc\")"), Ok(Value::Int(0))); // 非数値は 0
    }

    #[test]
    fn io_nowは正のintを返す() {
        match eval("now()") {
            Ok(Value::Int(n)) => assert!(n > 0, "now() は正のはず: {}", n),
            other => panic!("Int を期待: {:?}", other),
        }
    }

    #[test]
    fn io_読めないファイルはエラー() {
        assert!(matches!(
            eval("read_file(\"/no/such/awa/file/xyz\")"),
            Err(AwaError::Io(_))
        ));
    }

    #[test]
    fn async_all_でfutureのリストを並行に待つ() {
        // awa 自身で書いた汎用・再帰の all（List(Future(a)) -> Future(List(a))）。
        // Future は構築時にスケジュール済みなので、順に await しても実時間は並行。
        // 注釈付き再帰関数なので all(rest) の戻り型（Future）が確定しメソッド解決できる。
        // let の関数型注釈だけで引数型もラムダへ流れる（軽量双方向推論）。
        let src = "type List(a) { Cons(a, List(a)); Nil }\n\
                   let all: fn(List(Future(a))) -> Future(List(a)) = fn(fs) { \
                     case fs { Nil -> pure(Nil); Cons(f, rest) -> do { x <- f; xs <- all(rest); pure(Cons(x, xs)) } } }\n\
                   let sum = fn(xs: List(Int)) -> Int { case xs { Nil -> 0; Cons(h, t) -> h + sum(t) } }\n\
                   sum(run(all(Cons(delay(10, 1), Cons(delay(5, 2), Cons(pure(3), Nil))))))";
        assert_eq!(eval(src), Ok(Value::Int(6)));
    }

    #[test]
    fn async_future注釈つき関数() {
        // Future(a) を型注釈に書ける（組込型）。delay の結果を返す関数。
        let src = "let step = fn(n: Int) -> Future(Int) { delay(1, n + 1) }\nrun(step(41))";
        assert_eq!(eval(src), Ok(Value::Int(42)));
    }

    #[test]
    fn 演算子優先順位() {
        assert_eq!(eval("1 + 2 * 3"), Ok(num(7.0)));
        assert_eq!(eval("(1 + 2) * 3"), Ok(num(9.0)));
        assert_eq!(eval("2 * (3 + 4) - 1"), Ok(num(13.0)));
    }

    #[test]
    fn 変数束縛は後続の文から参照できる() {
        assert_eq!(eval("let x = 5; let y = x * 2 + 1; x + y"), Ok(num(16.0)));
    }

    #[test]
    fn 単項マイナスは乗算より強く結合する() {
        assert_eq!(eval("-2 * 5"), Ok(num(-10.0)));
        assert_eq!(eval("--3"), Ok(num(3.0)));
    }

    #[test]
    fn caseは選ばれた腕だけを評価する() {
        assert_eq!(
            eval("let x = 0; case x == 0 { true -> 100; false -> 1 / x }"),
            Ok(num(100.0))
        );
    }

    #[test]
    fn 比較は加減算より弱く結合する() {
        assert_eq!(eval("1 + 2 == 3"), Ok(Value::Bool(true)));
    }

    #[test]
    fn 基数付き整数リテラル() {
        assert_eq!(eval("0xFF"), Ok(Value::Int(255)));
        assert_eq!(eval("0o17"), Ok(Value::Int(15)));
        assert_eq!(eval("0b1010"), Ok(Value::Int(10)));
        assert_eq!(eval("0xDEAD_BEEF"), Ok(Value::Int(3735928559)));
        assert_eq!(eval("0xffL"), Ok(Value::Long(255)));
    }

    #[test]
    fn 論理演算子と短絡評価() {
        assert_eq!(eval("true && false"), Ok(Value::Bool(false)));
        assert_eq!(eval("false || true"), Ok(Value::Bool(true)));
        // 短絡: 左で確定すれば右（0 除算）は評価されない。
        assert_eq!(eval("false && 1 / 0 == 0"), Ok(Value::Bool(false)));
        assert_eq!(eval("true || 1 / 0 == 0"), Ok(Value::Bool(true)));
        // 優先順位: && / || は == より緩い。
        assert_eq!(eval("1 == 1 && 2 == 2"), Ok(Value::Bool(true)));
        // 型エラー: 論理演算子の引数は Bool。
        assert!(eval("1 && true").is_err());
    }

    #[test]
    fn ゼロ除算はエラー() {
        assert_eq!(eval("1 / 0"), Err(AwaError::DivisionByZero));
    }

    #[test]
    fn 未定義変数はエラー() {
        assert_eq!(eval("q + 1"), Err(AwaError::UndefinedVariable("q".to_string())));
    }

    // Phase 2 以降、これらは eval ではなく型検査段階で（実行前に）捕まる。
    // 型推論は unify の失敗として CannotUnify を返す。
    #[test]
    fn 型エラー_boolに算術() {
        assert_eq!(
            eval("true + 1"),
            Err(AwaError::CannotUnify { left: "Bool".to_string(), right: "Int".to_string() })
        );
    }

    #[test]
    fn 型エラー_numとboolの比較() {
        assert_eq!(
            eval("1 == true"),
            Err(AwaError::CannotUnify { left: "Int".to_string(), right: "Bool".to_string() })
        );
    }

    #[test]
    fn repl_env持ち越し() {
        let mut env = eval::builtins_env();
        let mut checker = Checker::new();
        // 型環境も checker に持ち越されるので、2 行目の x も型検査を通る。
        assert_eq!(run_values("let x = 10", &mut env, &mut checker), Ok(vec![Value::Int(10)]));
        assert_eq!(run_values("x + 5", &mut env, &mut checker), Ok(vec![Value::Int(15)]));
    }

    #[test]
    fn 括弧の対応が壊れた入力() {
        // (1 + 2 ; → ) を期待した位置で終端（;）が来る
        assert_eq!(eval("(1 + 2;"), Err(AwaError::UnexpectedToken("Terminator".to_string())));
    }

    #[test]
    fn 基本的な関数呼び出し() {
        assert_eq!(eval("let add = fn(a: Int, b: Int) { a + b }; add(1, 2)"), Ok(num(3.0)));
    }

    #[test]
    fn クロージャは定義時の変数を捕捉する() {
        assert_eq!(
            eval("let make_adder = fn(n: Int) { fn(x) { x + n } }; let add5 = make_adder(5); add5(10)"),
            Ok(num(15.0))
        );
    }

    #[test]
    fn 高階関数_関数を引数に渡せる() {
        assert_eq!(
            eval("let inc = fn(x) { x + 1 }; let twice = fn(f, x) { f(f(x)) }; twice(inc, 0)"),
            Ok(num(2.0))
        );
    }

    #[test]
    fn 再帰関数() {
        assert_eq!(
            eval("let fact = fn(n) { case n == 0 { true -> 1; false -> n * fact(n - 1) } }; fact(5)"),
            Ok(num(120.0))
        );
    }

    #[test]
    fn 相互再帰_トップレベルで前方参照できる() {
        // is_even が後で定義される is_odd を前方参照する（相互再帰）。
        let src = "let is_even = fn(n: Int) -> Bool { case n == 0 { true -> true; false -> is_odd(n - 1) } }\n\
                   let is_odd = fn(n: Int) -> Bool { case n == 0 { true -> false; false -> is_even(n - 1) } }\n\
                   is_even(10)";
        assert_eq!(eval(src), Ok(Value::Bool(true)));
        let src2 = "let is_even = fn(n: Int) -> Bool { case n == 0 { true -> true; false -> is_odd(n - 1) } }\n\
                    let is_odd = fn(n: Int) -> Bool { case n == 0 { true -> false; false -> is_even(n - 1) } }\n\
                    is_odd(10)";
        assert_eq!(eval(src2), Ok(Value::Bool(false)));
    }

    #[test]
    fn ブロック内相互再帰はインタプリタで動く() {
        let src = "{\n\
                   let ev = fn(n: Int) -> Bool { case n == 0 { true -> true; false -> od(n - 1) } }\n\
                   let od = fn(n: Int) -> Bool { case n == 0 { true -> false; false -> ev(n - 1) } }\n\
                   ev(10)\n\
                   }";
        assert_eq!(eval(src), Ok(Value::Bool(true)));
    }

    #[test]
    fn ブロック内相互再帰はnativeで明確なエラー() {
        // 兄弟関数を捕獲すると参照が循環し RC GC と衝突するため native 未対応。
        let src = "{\n\
                   let ev = fn(n: Int) -> Bool { case n == 0 { true -> true; false -> od(n - 1) } }\n\
                   let od = fn(n: Int) -> Bool { case n == 0 { true -> false; false -> ev(n - 1) } }\n\
                   ev(4)\n\
                   }";
        let err = compile_to_llvm(src).unwrap_err();
        assert!(
            format!("{}", err.error).contains("トップレベル"),
            "got: {:?}",
            err.error
        );
    }

    #[test]
    fn codegen_相互再帰がネイティブ実行される() {
        let (out, code) = compile_run_status(
            "let is_even = fn(n: Int) -> Bool { case n == 0 { true -> true; false -> is_odd(n - 1) } }\n\
             let is_odd = fn(n: Int) -> Bool { case n == 0 { true -> false; false -> is_even(n - 1) } }\n\
             print(\"${is_even(10)}\")\n\
             print(\"${is_odd(10)}\")",
        );
        assert_eq!(out, "true\nfalse\n");
        assert_eq!(code, 0);
    }

    #[test]
    fn 末尾再帰はtcoで定数スタック() {
        // 20 万段。TCO（トランポリン）が無ければテストスレッドのスタックを溢れさせる深さ。
        let src = "let c = fn(n: Int, a: Int) -> Int { case n == 0 { true -> a; false -> c(n - 1, a + 1) } }; c(200000, 0)";
        assert_eq!(eval(src), Ok(Value::Int(200000)));
    }

    #[test]
    fn レキシカルスコープ_呼び出し場所の変数は見えない() {
        assert_eq!(
            eval("let f = fn() { x }; let x = 99; f()"),
            Err(AwaError::UndefinedVariable("x".to_string()))
        );
    }

    #[test]
    fn 引数の個数が合わないとエラー() {
        assert_eq!(
            eval("let add = fn(a: Int, b: Int) { a + b }; add(1)"),
            Err(AwaError::ArityMismatch { expected: 2, found: 1 })
        );
    }

    #[test]
    fn 関数でない値の呼び出しはエラー() {
        // 型検査で Num を fn(Num) -> _ と unify しようとして失敗する。
        assert!(matches!(eval("let x = 5; x(1)"), Err(AwaError::CannotUnify { .. })));
    }

    // ===== Scala 風の文法（ASI・ブロック・Unit・コメント）=====

    #[test]
    fn セミコロン無しでも単文は評価できる() {
        assert_eq!(eval("1 + 2"), Ok(num(3.0)));
    }

    #[test]
    fn 改行が文の区切りになる() {
        let mut env = eval::builtins_env();
        let mut checker = Checker::new();
        assert_eq!(
            run_values("let x = 5\nx + 1", &mut env, &mut checker),
            Ok(vec![Value::Int(5), Value::Int(6)])
        );
    }

    #[test]
    fn 演算子の直後の改行は継続扱い() {
        assert_eq!(eval("1 +\n2 +\n3"), Ok(num(6.0)));
    }

    #[test]
    fn 区切り無しで式が連続するとエラー() {
        assert_eq!(eval("1 2"), Err(AwaError::UnexpectedToken("Int(2)".to_string())));
    }

    #[test]
    fn ブロックの値は末尾の式() {
        assert_eq!(eval("let v = { let a = 2; a * 3 }; v"), Ok(num(6.0)));
    }

    #[test]
    fn ブロックは新しいスコープを作る() {
        assert_eq!(eval("let a = 1; { let a = 99; a }; a"), Ok(num(1.0)));
    }

    #[test]
    fn 末尾がletのブロックはunit() {
        assert_eq!(eval("{ let a = 1 }"), Ok(Value::Unit));
    }

    #[test]
    fn 空ブロックはunit() {
        assert_eq!(eval("{}"), Ok(Value::Unit));
    }

    #[test]
    fn 関数本体に複数文ブロック() {
        assert_eq!(
            eval("let f = fn(x) { let y = x * 2; y + 1 }; f(10)"),
            Ok(num(21.0))
        );
    }

    #[test]
    fn 行コメントは無視される() {
        assert_eq!(eval("1 + 2 // これは無視される\n"), Ok(num(3.0)));
    }

    #[test]
    fn ブロックコメントは無視される() {
        assert_eq!(eval("1 + /* 途中の */ 2"), Ok(num(3.0)));
    }

    #[test]
    fn 閉じないブロックコメントはエラー() {
        assert_eq!(eval("1 + /* 閉じない"), Err(AwaError::UnterminatedComment));
    }

    #[test]
    fn 括弧内は改行を無視して複数行に書ける() {
        assert_eq!(eval("let add = fn(a: Int, b: Int) { a + b }; add(\n1,\n2\n)"), Ok(num(3.0)));
    }

    // ===== examples/ をテストとして実行する =====
    // include_str! でコンパイル時に埋め込むので、例が壊れるとビルド時の
    // テストで気づける（例とテストを二重管理しない）。

    #[test]
    fn example_polymorphism() {
        let src = include_str!("../../../examples/polymorphism.awa");
        let mut env = eval::builtins_env();
        let mut checker = Checker::new();
        let rows = run(src, &mut env, &mut checker).expect("型検査・評価が通るはず");
        // 末尾は fact(6) = 720 : Num。
        assert_eq!(rows.last().map(|(v, _)| v.clone()), Some(num(720.0)));
    }

    #[test]
    fn example_adt() {
        let src = include_str!("../../../examples/adt.awa");
        let mut env = eval::builtins_env();
        let mut checker = Checker::new();
        let rows = run(src, &mut env, &mut checker).expect("型検査・評価が通るはず");
        // 末尾は sum(Cons(1, Cons(2, Cons(3, Nil)))) = 6。
        assert_eq!(rows.last().map(|(v, _)| v.clone()), Some(num(6.0)));
    }

    // ===== ADT / パターンマッチ（Phase 3a）=====

    #[test]
    fn caseで構築子を分解する() {
        assert_eq!(
            eval("type Opt(a) { Some(a); None }; let x = Some(3); case x { Some(n) -> n + 1; None -> 0 }"),
            Ok(num(4.0))
        );
    }

    #[test]
    fn ヌル構築子にマッチする() {
        assert_eq!(
            eval("type Opt(a) { Some(a); None }; case None { Some(n) -> n; None -> 42 }"),
            Ok(num(42.0))
        );
    }

    #[test]
    fn リテラルパターンの網羅漏れは静的に弾く() {
        // Phase 3b: Num は無限ドメインなので catch-all 無しは網羅的でない。
        // 実行前に NonExhaustiveMatch（witness は _）。
        assert!(matches!(
            eval("case 5 { 1 -> 10; 2 -> 20 }"),
            Err(AwaError::NonExhaustiveMatch { .. })
        ));
    }

    #[test]
    fn catch_allがあれば網羅的() {
        assert_eq!(eval("case 5 { 1 -> 10; _ -> 0 }"), Ok(num(0.0)));
    }

    #[test]
    fn caseの腕の型が食い違うと型エラー() {
        // 一方の腕が Num、他方が Bool。本体型の unify で CannotUnify。
        assert!(matches!(
            eval("type Opt(a) { Some(a); None }; case Some(1) { Some(n) -> n; None -> true }"),
            Err(AwaError::CannotUnify { .. })
        ));
    }

    #[test]
    fn 未定義の構築子は型エラー() {
        assert_eq!(
            eval("case 1 { Ghost(x) -> x }"),
            Err(AwaError::UnknownConstructor("Ghost".to_string()))
        );
    }

    #[test]
    fn 構築子の引数個数違いは型エラー() {
        assert_eq!(
            eval("type Pair(a) { Two(a, a) }; Two(1)"),
            Err(AwaError::ArityMismatch { expected: 2, found: 1 })
        );
    }

    // ===== 網羅性・到達不能（Phase 3b）=====

    #[test]
    fn adtの網羅漏れは反例つきで弾く() {
        // None の腕が無い → witness は "None"。
        assert_eq!(
            eval("type Opt(a) { Some(a); None }; case Some(1) { Some(n) -> n }"),
            Err(AwaError::NonExhaustiveMatch { witness: "None".to_string() })
        );
    }

    #[test]
    fn boolの網羅漏れ() {
        assert_eq!(
            eval("case true { true -> 1 }"),
            Err(AwaError::NonExhaustiveMatch { witness: "false".to_string() })
        );
    }

    #[test]
    fn 入れ子の網羅漏れも検出する() {
        // Some(Nil) の腕が無い → witness は "Some(Nil)"。
        assert_eq!(
            eval(
                "type List(a) { Cons(a, List(a)); Nil }; type Opt(a) { Some(a); None }; \
                 case Some(Nil) { Some(Cons(h, t)) -> h; None -> 0 }"
            ),
            Err(AwaError::NonExhaustiveMatch { witness: "Some(Nil)".to_string() })
        );
    }

    #[test]
    fn ワイルドカードの後ろは到達不能() {
        assert_eq!(
            eval("type Opt(a) { Some(a); None }; case Some(1) { _ -> 0; Some(n) -> n; None -> 1 }"),
            Err(AwaError::UnreachablePattern("Some(n)".to_string()))
        );
    }

    #[test]
    fn 重複リテラルは到達不能() {
        assert_eq!(
            eval("case true { true -> 1; false -> 2; true -> 3 }"),
            Err(AwaError::UnreachablePattern("true".to_string()))
        );
    }

    #[test]
    fn 全構築子を覆えば網羅的() {
        assert_eq!(
            eval("type Opt(a) { Some(a); None }; case Some(7) { Some(n) -> n; None -> 0 }"),
            Ok(num(7.0))
        );
    }

    // ===== 型注釈（let / 引数 / 戻り値）=====

    #[test]
    fn example_annotations() {
        let src = include_str!("../../../examples/annotations.awa");
        let mut env = eval::builtins_env();
        let mut checker = Checker::new();
        let rows = run(src, &mut env, &mut checker).expect("型検査・評価が通るはず");
        // 末尾は first(None, 0) = 0。
        assert_eq!(rows.last().map(|(v, _)| v.clone()), Some(num(0.0)));
    }

    #[test]
    fn let注釈が一致すれば通る() {
        assert_eq!(eval("let x: Int = 5; x"), Ok(num(5.0)));
    }

    #[test]
    fn let注釈の食い違いは型エラー() {
        assert!(matches!(
            eval("let x: Int = true"),
            Err(AwaError::CannotUnify { .. })
        ));
    }

    #[test]
    fn 引数注釈が呼び出しを制約する() {
        // n: Num と注釈したので Bool を渡すと型エラー。
        assert!(matches!(
            eval("let f = fn(n: Int) { n }; f(true)"),
            Err(AwaError::CannotUnify { .. })
        ));
    }

    #[test]
    fn 戻り型注釈の食い違いは型エラー() {
        assert!(matches!(
            eval("let f = fn(n: Int) -> Bool { n + 1 }"),
            Err(AwaError::CannotUnify { .. })
        ));
    }

    #[test]
    fn 注釈の型変数は多相を保つ() {
        // fn(a) -> a と注釈しても多相のまま、Num と Bool 双方に使える。
        assert_eq!(
            eval("let id: fn(a) -> a = fn(x) { x }; id(1); id(true)"),
            Ok(Value::Bool(true))
        );
    }

    #[test]
    fn 一般的すぎる注釈は弾く() {
        // 恒等関数の実際の型は fn(a) -> a。fn(a) -> b と注釈するのは過度に一般的。
        assert!(matches!(
            eval("let f: fn(a) -> b = fn(x) { x }"),
            Err(AwaError::AnnotationTooGeneral { .. })
        ));
    }

    #[test]
    fn 未定義の型を注釈に書くとエラー() {
        assert_eq!(
            eval("let x: Ghost = 1"),
            Err(AwaError::UnknownType("Ghost".to_string()))
        );
    }

    // ===== or パターン =====

    #[test]
    fn orパターンはどれかに当たる() {
        assert_eq!(eval("case 2 { 1 | 2 | 3 -> 100; _ -> 0 }"), Ok(num(100.0)));
        assert_eq!(eval("case 9 { 1 | 2 | 3 -> 100; _ -> 0 }"), Ok(num(0.0)));
    }

    #[test]
    fn orで全構築子を覆えば網羅的() {
        assert_eq!(
            eval("type Color { Red; Green; Blue }; case Red { Red | Green | Blue -> 1 }"),
            Ok(num(1.0))
        );
    }

    #[test]
    fn orが一部しか覆わないと網羅漏れ() {
        assert_eq!(
            eval("type Color { Red; Green; Blue }; case Red { Red | Green -> 1 }"),
            Err(AwaError::NonExhaustiveMatch { witness: "Blue".to_string() })
        );
    }

    #[test]
    fn orの選択肢で束縛が食い違うとエラー() {
        assert_eq!(
            eval("type Opt(a) { Some(a); None }; case Some(1) { Some(x) | None -> x }"),
            Err(AwaError::OrPatternBindings)
        );
    }

    #[test]
    fn or選択肢が同じ変数を束縛すれば通る() {
        // A(x) も B(x) も x: Num を束縛 → 本体で x を使える。
        assert_eq!(
            eval("type T { A(Int); B(Int) }; let g = fn(t) { case t { A(x) | B(x) -> x } }; g(B(9))"),
            Ok(num(9.0))
        );
    }

    // ===== エラーの位置情報（Phase 4）=====

    // run を直接呼んで位置つきエラーを得るヘルパ。
    fn run_err(src: &str) -> LocatedError {
        let mut env = eval::builtins_env();
        let mut checker = Checker::new();
        run(src, &mut env, &mut checker).unwrap_err()
    }

    #[test]
    fn let注釈の引数型がラムダへ流れる() {
        // let に関数型注釈があれば、ラムダ引数に注釈が無くても型が決まる（軽量双方向推論）。
        assert_eq!(eval("let double: fn(Int) -> Int = fn(x) { x * 2 }\ndouble(21)"), Ok(Value::Int(42)));
        // 引数が複数でも流れる。
        assert_eq!(
            eval("let add: fn(Int, Int) -> Int = fn(a, b) { a + b }\nadd(20, 22)"),
            Ok(Value::Int(42))
        );
    }

    #[test]
    fn 型注釈の未定義型に位置がつく() {
        // 注釈 `x: Nope` の Nope（型式 span）を指す。以前は位置なし（None）だった。
        let src = "let f = fn(x: Nope) -> Int { 1 }\nf(0)";
        let err = run_err(src);
        assert!(matches!(err.error, AwaError::UnknownType(ref n) if n == "Nope"));
        assert!(err.span.is_some(), "型注釈エラーに位置がない");
        let msg = report(&err, src);
        assert!(msg.contains("1:15"), "Nope の位置(1:15)を指していない: {}", msg);
        assert!(msg.contains("^"));
    }

    #[test]
    fn 構文エラーに位置がつく() {
        let err = run_err("let y = )");
        assert!(err.span.is_some());
        // ) は 9 文字目（1 始まり）。
        assert!(report(&err, "let y = )").contains("1:9"));
    }

    #[test]
    fn 型エラーに位置がつく() {
        // true + 1 の二項演算ノード（1:1 から）を指す。
        let err = run_err("true + 1");
        assert!(matches!(err.error, AwaError::CannotUnify { .. }));
        let msg = report(&err, "true + 1");
        assert!(msg.contains("--> 1:1"));
        assert!(msg.contains("^"));
    }

    #[test]
    fn 未定義変数に位置がつく() {
        let err = run_err("1 + zzz");
        assert_eq!(err.error, AwaError::UndefinedVariable("zzz".to_string()));
        // zzz は 5 文字目。
        assert!(report(&err, "1 + zzz").contains("1:5"));
    }

    #[test]
    fn 実行時エラーに位置がつく() {
        // 型は通り、評価時にゼロ除算。位置つきで報告される。
        let err = run_err("10 / 0");
        assert_eq!(err.error, AwaError::DivisionByZero);
        assert!(err.span.is_some());
    }

    // ===== パイプ演算子 |>（Phase 5）=====

    #[test]
    fn example_pipe() {
        let src = include_str!("../../../examples/pipe.awa");
        let mut env = eval::builtins_env();
        let mut checker = Checker::new();
        let rows = run(src, &mut env, &mut checker).expect("型検査・評価が通るはず");
        // 末尾は double(add(1, 10)) = 22。
        assert_eq!(rows.last().map(|(v, _)| v.clone()), Some(num(22.0)));
    }

    #[test]
    fn パイプは関数適用に展開される() {
        assert_eq!(eval("let inc = fn(n) { n + 1 }; 5 |> inc"), Ok(num(6.0)));
    }

    #[test]
    fn パイプは左辺を第一引数に挿入する() {
        assert_eq!(eval("let add = fn(a: Int, b: Int) { a + b }; 1 |> add(2)"), Ok(num(3.0)));
    }

    #[test]
    fn パイプは左結合で連鎖する() {
        assert_eq!(
            eval("let inc = fn(n) { n + 1 }; let add = fn(a: Int, b: Int) { a + b }; 1 |> add(2) |> inc"),
            Ok(num(4.0))
        );
    }

    #[test]
    fn パイプは算術より緩い() {
        // 1 + 2 |> inc は inc(1 + 2) = 4。
        assert_eq!(eval("let inc = fn(n) { n + 1 }; 1 + 2 |> inc"), Ok(num(4.0)));
    }

    #[test]
    fn パイプは多行で書ける() {
        // 行末 |> で継続。
        assert_eq!(
            eval("let inc = fn(n) { n + 1 }; let double = fn(n) { n * 2 }; 1 |>\ninc |>\ndouble"),
            Ok(num(4.0))
        );
    }

    // ===== 限定メソッド（Phase 6）=====

    const OPT: &str = "type Opt(a) { Some(a); None }; \
        impl Opt(a) { \
          fn map(self, f) { case self { Some(x) -> Some(f(x)); None -> None } }; \
          fn unwrap_or(self, d) { case self { Some(x) -> x; None -> d } } \
        }; let inc = fn(n) { n + 1 }; ";

    #[test]
    fn example_methods() {
        let src = include_str!("../../../examples/methods.awa");
        let mut env = eval::builtins_env();
        let mut checker = Checker::new();
        let rows = run(src, &mut env, &mut checker).expect("型検査・評価が通るはず");
        assert_eq!(rows.last().map(|(v, _)| v.clone()), Some(num(7.0)));
    }

    #[test]
    fn メソッド呼び出しとチェーン() {
        assert_eq!(eval(&format!("{OPT} Some(5).map(inc).unwrap_or(0)")), Ok(num(6.0)));
        assert_eq!(eval(&format!("{OPT} None.map(inc).unwrap_or(99)")), Ok(num(99.0)));
    }

    #[test]
    fn 組込型のメソッド() {
        assert_eq!(
            eval("impl Int { fn double(self) { self * 2 } }; 5.double()"),
            Ok(num(10.0))
        );
    }

    #[test]
    fn 自己再帰メソッド() {
        assert_eq!(
            eval(
                "type Tree(a) { Leaf(a); Branch(Tree(a), Tree(a)) }; \
                 impl Tree(a) { fn size(self) { case self { Leaf(x) -> 1; Branch(l, r) -> l.size() + r.size() } } }; \
                 Branch(Leaf(1), Branch(Leaf(2), Leaf(3))).size()"
            ),
            Ok(num(3.0))
        );
    }

    #[test]
    fn 未確定レシーバはエラー() {
        // x の型が定まらないのでメソッドを解決できない。
        assert_eq!(
            eval("impl Int { fn double(self) { self * 2 } }; fn(x) { x.double() }"),
            Err(AwaError::MethodReceiverUnknown)
        );
    }

    #[test]
    fn 引数注釈でレシーバが定まれば解決する() {
        assert_eq!(
            eval("impl Int { fn double(self) { self * 2 } }; let f = fn(x: Int) { x.double() }; f(5)"),
            Ok(num(10.0))
        );
    }

    #[test]
    fn 未定義メソッドはエラー() {
        assert_eq!(
            eval(&format!("{OPT} Some(1).ghost()")),
            Err(AwaError::UnknownMethod { ty: "Opt".to_string(), method: "ghost".to_string() })
        );
    }

    // ===== do 記法（Phase 7）=====

    // flatMap を持つ Option（do はこれへ脱糖する）。
    const MOPT: &str = "type Opt(a) { Some(a); None }; \
        impl Opt(a) { fn flatMap(self, f) { case self { Some(x) -> f(x); None -> None } } }; ";

    #[test]
    fn example_do() {
        let src = include_str!("../../../examples/do.awa");
        let mut env = eval::builtins_env();
        let mut checker = Checker::new();
        let rows = run(src, &mut env, &mut checker).expect("型検査・評価が通るはず");
        // 末尾 pair = Some(30)。Data 比較で確認。
        match rows.last().map(|(v, _)| v.clone()) {
            Some(Value::Data { ctor, args }) => {
                assert_eq!(ctor, "Some");
                assert_eq!(args, vec![num(30.0)]);
            }
            other => panic!("Some(30) のはず: {:?}", other),
        }
    }

    #[test]
    fn doは全て成功なら計算が進む() {
        assert!(matches!(
            eval(&format!("{MOPT} do {{ x <- Some(2); y <- Some(3); Some(x + y) }}")),
            Ok(Value::Data { .. })
        ));
    }

    #[test]
    fn doは失敗で短絡する() {
        // 途中の None.flatMap(...) = None。具体的なレシーバで解決される。
        assert_eq!(
            eval(&format!("{MOPT} do {{ x <- Some(5); y <- None; Some(x + y) }}")),
            eval(&format!("{MOPT} None"))
        );
    }

    // ===== 標準型 Int/Long/Float/String（Phase 8）=====

    #[test]
    fn example_types() {
        let src = include_str!("../../../examples/types.awa");
        let mut env = eval::builtins_env();
        let mut checker = Checker::new();
        let rows = run(src, &mut env, &mut checker).expect("型検査・評価が通るはず");
        assert_eq!(rows.last().map(|(v, _)| v.clone()), Some(Value::Int(1)));
    }

    #[test]
    fn 整数と浮動小数は別の型() {
        assert_eq!(eval("1 + 2"), Ok(Value::Int(3)));
        assert_eq!(eval("1.0 + 2.0"), Ok(Value::Float(3.0)));
        assert_eq!(eval("1L + 2L"), Ok(Value::Long(3)));
    }

    #[test]
    fn 数値型の混用は型エラー() {
        assert!(matches!(eval("1 + 1.0"), Err(AwaError::CannotUnify { .. })));
        assert!(matches!(eval("1 + 1L"), Err(AwaError::CannotUnify { .. })));
    }

    #[test]
    fn 数値でない算術はnotnumeric() {
        assert_eq!(eval("true + false"), Err(AwaError::NotNumeric("Bool".to_string())));
    }

    #[test]
    fn 注釈なしの算術は曖昧() {
        assert_eq!(
            eval("let add = fn(a, b) { a + b }; 0"),
            Err(AwaError::AmbiguousNumeric)
        );
    }

    #[test]
    fn 文字列リテラルと連結() {
        assert_eq!(eval(r#""ab" <> "cd""#), Ok(Value::Str("abcd".to_string())));
        assert_eq!(eval(r#""x" == "x""#), Ok(Value::Bool(true)));
    }

    #[test]
    fn 三連引用符は複数行生文字列() {
        // 改行はそのまま保持される。
        assert_eq!(eval("\"\"\"a\nb\"\"\""), Ok(Value::Str("a\nb".to_string())));
        // 生（エスケープ非解釈）: \n はバックスラッシュ+n のまま。
        assert_eq!(eval("\"\"\"x\\ny\"\"\""), Ok(Value::Str("x\\ny".to_string())));
        // 内部の " はそのまま含められる（末尾以外）。
        assert_eq!(eval("\"\"\"say \"hi\" loud\"\"\""), Ok(Value::Str("say \"hi\" loud".to_string())));
    }

    #[test]
    fn 文字列補間は型駆動で文字列化する() {
        // String はそのまま、Int/Float/Bool は自動で文字列化。
        assert_eq!(
            eval(r#"let name = "awa"; "hi ${name}!""#),
            Ok(Value::Str("hi awa!".to_string()))
        );
        assert_eq!(
            eval(r#"let n = 40; "n=${n + 2}""#),
            Ok(Value::Str("n=42".to_string()))
        );
        assert_eq!(
            eval(r#""f=${3.5}""#),
            Ok(Value::Str("f=3.5".to_string()))
        );
        assert_eq!(
            eval(r#""b=${1 < 2}""#),
            Ok(Value::Str("b=true".to_string()))
        );
        // 複数の補間と前後リテラル。
        assert_eq!(
            eval(r#"let x = 1; let y = 2; "${x}+${y}=${x + y}""#),
            Ok(Value::Str("1+2=3".to_string()))
        );
        // 補間が無ければ従来どおり単一の文字列。
        assert_eq!(eval(r#""plain""#), Ok(Value::Str("plain".to_string())));
        // \$ で $ をリテラルに。
        assert_eq!(
            eval(r#""price \${10}""#),
            Ok(Value::Str("price ${10}".to_string()))
        );
    }

    #[test]
    fn 文字列プリミティブ() {
        // string_char_at: i 番目のコードポイント（範囲外 -1）。UTF-8 をコードポイント単位で。
        assert_eq!(
            eval(r#"char_to_string(string_char_at("héllo", 1))"#),
            Ok(Value::Str("é".to_string()))
        );
        assert_eq!(eval(r#"string_char_at("ab", 5)"#), Ok(Value::Int(-1)));
        assert_eq!(eval(r#"string_char_at("héllo", 4)"#), Ok(Value::Int(111))); // 'o'
        // char_to_string: コードポイント → 1 文字（BMP 外も）。
        assert_eq!(eval("char_to_string(128512)"), Ok(Value::Str("😀".to_string())));
        // string_to_float: 先頭の数値部を読む。
        assert_eq!(eval(r#"string_to_float("3.5")"#), Ok(Value::Float(3.5)));
        assert_eq!(eval(r#"string_to_float("nope")"#), Ok(Value::Float(0.0)));
    }

    #[test]
    fn codegen_文字列プリミティブがネイティブ実行される() {
        // 同じ式が native でもインタプリタと一致する（UTF-8 往復・float パース）。
        let (out, code) = compile_run_status(
            "print(char_to_string(string_char_at(\"héllo\", 1)))\n\
             print(int_to_string(string_char_at(\"ab\", 5)))\n\
             print(float_to_string(string_to_float(\"3.5\")))\n\
             print(char_to_string(128512))",
        );
        assert_eq!(out, "é\n-1\n3.5\n😀\n");
        assert_eq!(code, 0);
    }

    #[test]
    fn std_string_slice_split() {
        // std/string の slice/index_of/starts_with/split を native で。
        let out = std_build_run(
            "import std/io\n\
             import std/string\n\
             import std/list\n\
             io.println(string.slice(\"hello world\", 0, 5))\n\
             io.println(\"${string.index_of(\"a,b,c\", 44)}\")\n\
             io.println(\"${string.starts_with(\"hello\", \"he\")}\")\n\
             io.println(\"${list.length(string.split(\"a,b,c\", 44))}\")\n",
        );
        assert_eq!(out, "hello\n1\ntrue\n3\n");
    }

    #[test]
    fn 三連引用符でも補間できる() {
        assert_eq!(
            eval("let n = 7;\n\"\"\"val=${n}\nend\"\"\""),
            Ok(Value::Str("val=7\nend".to_string()))
        );
    }

    #[test]
    fn 数値変換の組込関数() {
        assert_eq!(eval("int_to_float(3)"), Ok(Value::Float(3.0)));
        assert_eq!(eval("int_to_string(5)"), Ok(Value::Str("5".to_string())));
        // 変換後は同じ型同士で演算できる。
        assert_eq!(eval("int_to_float(3) + 1.0"), Ok(Value::Float(4.0)));
    }

    #[test]
    fn 文字列パターンマッチ() {
        assert_eq!(
            eval(r#"case "yes" { "yes" -> 1; _ -> 0 }"#),
            Ok(Value::Int(1))
        );
    }

    // ===== コード生成（Phase 9）=====
    // clang で実際にネイティブ化して実行し、標準出力を検査する（clang 前提）。
    use std::sync::atomic::{AtomicU64, Ordering};
    static CODEGEN_CTR: AtomicU64 = AtomicU64::new(0);

    fn compile_and_run(src: &str) -> String {
        let ir = compile_to_llvm(src).expect("コンパイルが通るはず");
        let id = std::process::id() as u64 * 1_000_000
            + CODEGEN_CTR.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir();
        let ll = dir.join(format!("awa_cg_{}.ll", id));
        let rt = dir.join(format!("awa_rt_{}.c", id));
        let exe = dir.join(format!("awa_cg_{}", id));
        std::fs::write(&ll, &ir).unwrap();
        std::fs::write(&rt, codegen::ASYNC_RUNTIME_C).unwrap();
        let st = std::process::Command::new("clang")
            .arg("-O2")
            .arg(&rt)
            .arg(&ll)
            .arg("-o")
            .arg(&exe)
            .status()
            .expect("clang を起動できる");
        assert!(st.success(), "clang が失敗");
        let out = std::process::Command::new(&exe).output().expect("実行できる");
        let _ = std::fs::remove_file(&ll);
        let _ = std::fs::remove_file(&rt);
        let _ = std::fs::remove_file(&exe);
        String::from_utf8(out.stdout).unwrap()
    }

    // stdout と終了コードの両方を返す（終了コード = 残存ヒープ数 = リーク数）。
    fn compile_run_status(src: &str) -> (String, i32) {
        let ir = compile_to_llvm(src).expect("コンパイルが通るはず");
        let id = std::process::id() as u64 * 1_000_000
            + CODEGEN_CTR.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir();
        let ll = dir.join(format!("awa_cg_{}.ll", id));
        let rt = dir.join(format!("awa_rt_{}.c", id));
        let exe = dir.join(format!("awa_cg_{}", id));
        std::fs::write(&ll, &ir).unwrap();
        std::fs::write(&rt, codegen::ASYNC_RUNTIME_C).unwrap();
        let st = std::process::Command::new("clang")
            .arg("-O2").arg(&rt).arg(&ll).arg("-o").arg(&exe).status().expect("clang を起動できる");
        assert!(st.success(), "clang が失敗");
        let out = std::process::Command::new(&exe).output().expect("実行できる");
        let _ = std::fs::remove_file(&ll);
        let _ = std::fs::remove_file(&rt);
        let _ = std::fs::remove_file(&exe);
        (String::from_utf8(out.stdout).unwrap(), out.status.code().unwrap_or(-1))
    }

    // 複数モジュールを一時ディレクトリに書き出し、ローダ経由でビルド・実行する。
    // files = [(相対パス, 内容)]、entry = エントリのモジュールパス（"main" など）。
    fn compile_modules_status(files: &[(&str, &str)], entry: &str) -> (String, i32) {
        let id = std::process::id() as u64 * 1_000_000
            + CODEGEN_CTR.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!("awa_mod_{}", id));
        std::fs::create_dir_all(&dir).unwrap();
        for (rel, content) in files {
            let p = dir.join(rel);
            if let Some(parent) = p.parent() {
                std::fs::create_dir_all(parent).unwrap();
            }
            std::fs::write(&p, content).unwrap();
        }
        let modules = load_program(&dir, None, entry).expect("モジュール読み込み成功");
        let ir = compile_modules_to_ir(&modules).expect("コンパイル成功");
        let ll = dir.join("out.ll");
        let rt = dir.join("async_rt.c");
        let exe = dir.join("out");
        std::fs::write(&ll, &ir).unwrap();
        std::fs::write(&rt, codegen::ASYNC_RUNTIME_C).unwrap();
        let st = std::process::Command::new("clang")
            .arg("-O2").arg(&rt).arg(&ll).arg("-o").arg(&exe).status().expect("clang を起動できる");
        assert!(st.success(), "clang が失敗");
        let out = std::process::Command::new(&exe).output().expect("実行できる");
        let _ = std::fs::remove_dir_all(&dir);
        (String::from_utf8(out.stdout).unwrap(), out.status.code().unwrap_or(-1))
    }

    #[test]
    fn module_修飾呼び出しがネイティブ実行される() {
        // util の pub 関数を main から util.double で呼ぶ。
        let (out, code) = compile_modules_status(
            &[
                ("util.awa", "pub let double = fn(n: Int) -> Int { n * 2 }\n"),
                ("main.awa", "import util\nutil.double(21)\n"),
            ],
            "main",
        );
        assert_eq!(out, "42\n");
        assert_eq!(code, 0);
    }

    #[test]
    fn module_非公開関数の参照はエラー() {
        // pub の無い関数を他モジュールから参照すると NotExported。
        let id = std::process::id() as u64 * 1_000_000
            + CODEGEN_CTR.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!("awa_mod_{}", id));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("util.awa"), "let secret = fn(n: Int) -> Int { n }\n").unwrap();
        std::fs::write(dir.join("main.awa"), "import util\nutil.secret(1)\n").unwrap();
        let modules = load_program(&dir, None, "main").expect("読み込み成功");
        let err = check_program_modules(&modules).unwrap_err();
        let _ = std::fs::remove_dir_all(&dir);
        assert!(err.contains("公開されていません"), "実際のエラー: {}", err);
    }

    #[test]
    fn module_越境adtの構築と関数呼び出し() {
        // shapes の pub 型を main が構築し、shapes 内で case する関数を呼ぶ。
        let (out, code) = compile_modules_status(
            &[
                (
                    "shapes.awa",
                    "pub type Shape { Circle(Int); Square(Int) }\n\
                     pub let area = fn(s: Shape) -> Int { case s { Circle(r) -> r * r; Square(w) -> w * w } }\n",
                ),
                (
                    "main.awa",
                    "import shapes\n\
                     shapes.area(shapes.Circle(10))\n\
                     shapes.area(shapes.Square(5))\n",
                ),
            ],
            "main",
        );
        assert_eq!(out, "100\n25\n");
        assert_eq!(code, 0);
    }

    #[test]
    fn module_越境型注釈とパターンとヌル構築子() {
        // opt.Opt(Int) 注釈・opt.Some(x)/opt.None パターン・opt.Some(42)/opt.None 構築。
        let (out, code) = compile_modules_status(
            &[
                ("opt.awa", "pub type Opt(a) { Some(a); None }\n"),
                (
                    "main.awa",
                    "import opt\n\
                     let f = fn(o: opt.Opt(Int)) -> Int { case o { opt.Some(x) -> x; opt.None -> 0 } }\n\
                     f(opt.Some(42))\n\
                     f(opt.None)\n",
                ),
            ],
            "main",
        );
        assert_eq!(out, "42\n0\n");
        assert_eq!(code, 0);
    }

    #[test]
    fn module_import別名で参照できる() {
        let (out, code) = compile_modules_status(
            &[
                ("util.awa", "pub let double = fn(n: Int) -> Int { n * 2 }\n"),
                ("main.awa", "import util as u\nu.double(21)\n"),
            ],
            "main",
        );
        assert_eq!(out, "42\n");
        assert_eq!(code, 0);
    }

    #[test]
    fn module_修飾値参照を高階とpipeで使える() {
        // util.double を「値として」高階関数へ渡し、pipe の右辺にも置く。
        let (out, code) = compile_modules_status(
            &[
                ("util.awa", "pub let double = fn(n: Int) -> Int { n * 2 }\n"),
                (
                    "main.awa",
                    "import util\n\
                     let apply = fn(f: fn(Int) -> Int, x: Int) -> Int { f(x) }\n\
                     apply(util.double, 21)\n\
                     10 |> util.double\n",
                ),
            ],
            "main",
        );
        assert_eq!(out, "42\n20\n");
        assert_eq!(code, 0);
    }

    #[test]
    fn module_越境do記法() {
        // 別モジュールの Opt とその flatMap を import して do を書く（成功と短絡の両方）。
        let opt = "pub type Opt(a) { Some(a); None }\n\
                   pub impl Opt(a) { fn flatMap(self, f: fn(a) -> Opt(b)) -> Opt(b) { case self { Some(x) -> f(x); None -> None } } }\n";
        let (ok, code1) = compile_modules_status(
            &[
                ("opt.awa", opt),
                (
                    "main.awa",
                    "import opt\n\
                     let r = do { x <- opt.Some(10); y <- opt.Some(20); opt.Some(x + y) }\n\
                     case r { opt.Some(n) -> n; opt.None -> 0 }\n",
                ),
            ],
            "main",
        );
        assert_eq!(ok, "30\n");
        assert_eq!(code1, 0);
        // 途中 None で短絡。
        let (sc, code2) = compile_modules_status(
            &[
                ("opt.awa", opt),
                (
                    "main.awa",
                    "import opt\n\
                     let r = do { x <- opt.Some(10); y <- opt.None; opt.Some(x + y) }\n\
                     case r { opt.Some(n) -> n; opt.None -> 0 }\n",
                ),
            ],
            "main",
        );
        assert_eq!(sc, "0\n");
        assert_eq!(code2, 0);
    }

    #[test]
    fn module_越境メソッド呼び出し() {
        // 別モジュールの型 + impl メソッドを import して recv.method() で呼ぶ。
        let (out, code) = compile_modules_status(
            &[
                (
                    "shapes.awa",
                    "pub type Shape { Circle(Int); Square(Int) }\n\
                     pub impl Shape { fn area(self) -> Int { case self { Circle(r) -> r * r; Square(w) -> w * w } } }\n",
                ),
                (
                    "main.awa",
                    "import shapes\n\
                     shapes.Circle(10).area()\n\
                     shapes.Square(4).area()\n",
                ),
            ],
            "main",
        );
        assert_eq!(out, "100\n16\n");
        assert_eq!(code, 0);
    }

    #[test]
    fn module_per_method_pub() {
        // pub fn だけ越境公開、無印 fn はモジュール内限定（pub メソッドからは呼べる）。
        let shapes = "pub type Shape { Circle(Int) }\n\
                      impl Shape {\n\
                        pub fn area(self) -> Int { case self { Circle(r) -> self.r2(r) } }\n\
                        fn r2(self, r: Int) -> Int { r * r }\n\
                      }\n";
        // pub fn area は越境呼び出しでき、内部で private r2 を使える。
        let (out, code) = compile_modules_status(
            &[("shapes.awa", shapes), ("main.awa", "import shapes\nshapes.Circle(6).area()\n")],
            "main",
        );
        assert_eq!(out, "36\n");
        assert_eq!(code, 0);
        // private な r2 の越境呼び出しは拒否。
        let id = std::process::id() as u64 * 1_000_000
            + CODEGEN_CTR.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!("awa_mod_{}", id));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("shapes.awa"), shapes).unwrap();
        std::fs::write(dir.join("main.awa"), "import shapes\nshapes.Circle(6).r2(3)\n").unwrap();
        let modules = load_program(&dir, None, "main").expect("読み込み成功");
        let err = check_program_modules(&modules).unwrap_err();
        let _ = std::fs::remove_dir_all(&dir);
        assert!(err.contains("非公開"), "実際のエラー: {}", err);
    }

    #[test]
    fn module_非公開メソッドの越境呼び出しはエラー() {
        // pub なし impl のメソッドを他モジュールから呼ぶと PrivateMethod。
        let id = std::process::id() as u64 * 1_000_000
            + CODEGEN_CTR.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!("awa_mod_{}", id));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("shapes.awa"),
            "pub type Shape { Circle(Int) }\nimpl Shape { fn area(self) -> Int { case self { Circle(r) -> r * r } } }\n",
        )
        .unwrap();
        std::fs::write(dir.join("main.awa"), "import shapes\nshapes.Circle(5).area()\n").unwrap();
        let modules = load_program(&dir, None, "main").expect("読み込み成功");
        let err = check_program_modules(&modules).unwrap_err();
        let _ = std::fs::remove_dir_all(&dir);
        assert!(err.contains("非公開"), "実際のエラー: {}", err);
    }

    #[test]
    fn module_pub_implなら越境呼び出しできる() {
        // 同じ内容でも pub impl にすれば越境呼び出しが通る。同一モジュールの private メソッドは可。
        let (out, code) = compile_modules_status(
            &[
                (
                    "shapes.awa",
                    "pub type Shape { Circle(Int) }\npub impl Shape { fn area(self) -> Int { case self { Circle(r) -> r * r } } }\n",
                ),
                ("main.awa", "import shapes\nshapes.Circle(5).area()\n"),
            ],
            "main",
        );
        assert_eq!(out, "25\n");
        assert_eq!(code, 0);
    }

    #[test]
    fn module_エラー位置が元ファイルを指す() {
        // import 先のモジュールで起きた型エラーが、そのファイル名・行・列で報告される。
        let id = std::process::id() as u64 * 1_000_000
            + CODEGEN_CTR.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!("awa_mod_{}", id));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("util.awa"), "pub let bad = fn(n: Int) -> Int { n + true }\n").unwrap();
        std::fs::write(dir.join("main.awa"), "import util\nutil.bad(1)\n").unwrap();
        let modules = load_program(&dir, None, "main").expect("読み込み成功");
        let err = check_program_modules(&modules).unwrap_err();
        let _ = std::fs::remove_dir_all(&dir);
        // ファイル名 util と該当ソース行＋キャレットが含まれること。
        assert!(err.contains("util:1:"), "ファイル位置が出ていない: {}", err);
        assert!(err.contains("n + true"), "ソース行が出ていない: {}", err);
        assert!(err.contains('^'), "キャレットが出ていない: {}", err);
    }

    #[test]
    fn module_循環importはエラー() {
        let id = std::process::id() as u64 * 1_000_000
            + CODEGEN_CTR.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!("awa_mod_{}", id));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a.awa"), "import b\npub let f = fn(n: Int) -> Int { n }\n").unwrap();
        std::fs::write(dir.join("b.awa"), "import a\npub let g = fn(n: Int) -> Int { n }\n").unwrap();
        let err = load_program(&dir, None, "a").unwrap_err();
        let _ = std::fs::remove_dir_all(&dir);
        assert!(err.contains("循環"), "実際のエラー: {}", err);
    }

    #[test]
    fn codegen_メソッドがネイティブ実行される() {
        // impl のメソッドがトップレベル関数として codegen され、recv.method() が呼べる。
        let out = compile_and_run(
            "type Box { B(Int) }\n\
             impl Box { fn get(self) -> Int { case self { B(n) -> n } } }\n\
             B(7).get()\n\
             B(3).get()",
        );
        assert_eq!(out, "7\n3\n");
    }

    #[test]
    fn codegen_ジェネリックメソッドの単相化() {
        // 多相メソッドが使用具体型ごとに特殊化される。
        let out = compile_and_run(
            "type Opt(a) { Some(a); None }\n\
             impl Opt(a) { fn getOr(self, d: a) -> a { case self { Some(x) -> x; None -> d } } }\n\
             Some(5).getOr(0)\n\
             None.getOr(99)",
        );
        assert_eq!(out, "5\n99\n");
    }

    #[test]
    fn codegen_do記法がネイティブ実行される() {
        // do は flatMap メソッド呼び出しへ脱糖され、メソッド codegen に乗ってネイティブ実行される。
        let out = compile_and_run(
            "type Opt(a) { Some(a); None }\n\
             impl Opt(a) { fn flatMap(self, f: fn(a) -> Opt(b)) -> Opt(b) { case self { Some(x) -> f(x); None -> None } } }\n\
             let r = do { x <- Some(2); y <- Some(3); Some(x + y) }\n\
             case r { Some(n) -> n; None -> 0 }",
        );
        assert_eq!(out, "5\n");
    }

    #[test]
    fn gc_文字列がリークしない() {
        // 文字列の生成・連結・比較・グローバル束縛を行い、終了コード（残存数）が 0 になること。
        let (out, code) = compile_run_status(
            "let a = \"foo\" <> \"bar\"\n\
             let b = int_to_string(42)\n\
             a <> b\n\
             b == \"42\"\n\
             \"hello\"",
        );
        assert_eq!(out, "foobar42\ntrue\nhello\n");
        assert_eq!(code, 0, "文字列がリークしている（残存ヒープ数 {}）", code);
    }

    #[test]
    fn gc_再帰adtがリークしない() {
        // List(Int) を構築して畳み込み、drop_fn の連鎖解放で残存数が 0 になること。
        let (out, code) = compile_run_status(
            "type List(a) { Cons(a, List(a)); Nil }\n\
             let sum = fn(xs: List(Int)) -> Int { case xs { Nil -> 0; Cons(h, t) -> h + sum(t) } }\n\
             sum(Cons(1, Cons(2, Cons(3, Nil))))",
        );
        assert_eq!(out, "6\n");
        assert_eq!(code, 0, "ADT がリークしている（残存ヒープ数 {}）", code);
    }

    #[test]
    fn gc_文字列フィールドを持つadtがリークしない() {
        // Some(String) の case：取り出した文字列は生存し、Some ブロックの drop が中身を解放。
        let (out, code) = compile_run_status(
            "type Opt(a) { Some(a); None }\n\
             let get = fn(o: Opt(String), d: String) -> String { case o { Some(s) -> s; None -> d } }\n\
             get(Some(\"hi\" <> \"!\"), \"def\")\n\
             get(None, \"fallback\")",
        );
        assert_eq!(out, "hi!\nfallback\n");
        assert_eq!(code, 0, "文字列フィールド付き ADT がリークしている（残存 {}）", code);
    }

    #[test]
    fn gc_クロージャがリークしない() {
        // make_adder のクロージャ（env を malloc）を複数回呼び、env が最後に解放されること。
        let (out, code) = compile_run_status(
            "let mk = fn(n: Int) { fn(x: Int) -> Int { x + n } }\n\
             let add5 = mk(5)\n\
             add5(10)\n\
             add5(20)",
        );
        assert_eq!(out, "15\n25\n");
        assert_eq!(code, 0, "クロージャ env がリークしている（残存 {}）", code);
    }

    #[test]
    fn gc_文字列を捕獲したクロージャがリークしない() {
        // env が動的文字列を捕獲 → env の drop_fn がそれを解放すること。
        let (out, code) = compile_run_status(
            "let mk = fn(s: String) { fn(x: Int) -> String { s } }\n\
             let f = mk(\"a\" <> \"b\")\n\
             f(0)",
        );
        assert_eq!(out, "ab\n");
        assert_eq!(code, 0, "文字列を捕獲したクロージャがリークしている（残存 {}）", code);
    }

    #[test]
    fn codegen_再帰factがネイティブ実行される() {
        let out = compile_and_run(
            "let fact = fn(n: Int) -> Int { case n == 0 { true -> 1; false -> n * fact(n - 1) } }; fact(5)",
        );
        assert_eq!(out, "120\n");
    }

    #[test]
    fn codegen_floatとlong() {
        // Float の fadd/fdiv と Long の整数演算がネイティブ実行される。
        let out = compile_and_run("1.0 + 0.5\n7.0 / 2.0\n5L + 6L");
        assert_eq!(out, "1.5\n3.5\n11\n");
    }

    #[test]
    fn codegen_算術と比較() {
        // 7, true の順に出力される。
        let out = compile_and_run("let add = fn(a: Int, b: Int) -> Int { a + b }; add(3, 4)\n1 < 2");
        assert_eq!(out, "7\ntrue\n");
    }

    #[test]
    fn codegen_クロージャと高階関数() {
        // make_adder のキャプチャ（malloc env）と apply_twice の間接呼び出しが native 実行される。
        let out = compile_and_run(
            "let make_adder = fn(n: Int) { fn(x: Int) -> Int { x + n } }; \
             let add5 = make_adder(5); \
             add5(10)\n\
             let inc = fn(n: Int) -> Int { n + 1 }; \
             let twice = fn(f: fn(Int) -> Int, x: Int) -> Int { f(f(x)) }; \
             twice(inc, 0)",
        );
        assert_eq!(out, "15\n2\n");
    }

    #[test]
    fn codegen_単相化() {
        // 総称関数 id を Int と Bool で使う → それぞれ特殊化されてネイティブ実行。
        let out = compile_and_run("let id = fn(x) { x }; id(5)\nid(7)");
        assert_eq!(out, "5\n7\n");
    }

    #[test]
    fn codegen_総称関数と高階() {
        // compose（総称）に単相関数を渡して合成。inc(dbl(10)) = 21。
        let out = compile_and_run(
            "let compose = fn(f, g) { fn(x) { f(g(x)) } }; \
             let inc = fn(n: Int) -> Int { n + 1 }; \
             let dbl = fn(n: Int) -> Int { n * 2 }; \
             let h = compose(inc, dbl); h(10)",
        );
        assert_eq!(out, "21\n");
    }

    #[test]
    fn codegen_文字列と変換() {
        let out = compile_and_run(
            "\"hello\" <> \" \" <> \"world\"\n\"a\" == \"a\"\nint_to_string(42)\nint_to_float(3) + 1.5",
        );
        assert_eq!(out, "hello world\ntrue\n42\n4.5\n");
    }

    #[test]
    fn codegen_adtとパターンマッチ() {
        // Option の構築・case と、再帰 ADT(List)の畳み込みが native 実行される。
        let out = compile_and_run(
            "type Opt(a) { Some(a); None }; \
             let uw = fn(o: Opt(Int), d: Int) -> Int { case o { Some(x) -> x; None -> d } }; \
             uw(Some(42), 0)\n\
             uw(None, 7)\n\
             type List(a) { Cons(a, List(a)); Nil }; \
             let sum = fn(xs: List(Int)) -> Int { case xs { Nil -> 0; Cons(h, t) -> h + sum(t) } }; \
             sum(Cons(1, Cons(2, Cons(3, Nil))))",
        );
        assert_eq!(out, "42\n7\n6\n");
    }

    #[test]
    fn codegen_トップレベル値を関数から参照() {
        // トップレベルの値 let が LLVM グローバル変数になり、関数本体から参照できる。
        // 以前はキャプチャできず Unsupported だった。
        let out = compile_and_run(
            "let base = 100\n\
             let add_base = fn(n: Int) -> Int { n + base }\n\
             add_base(5)\n\
             base",
        );
        assert_eq!(out, "105\n100\n");
    }

    #[test]
    fn codegen_文字列パターン() {
        // case の文字列パターンが strcmp 経由で native 実行される。
        let out = compile_and_run(
            "let greet = fn(lang: String) -> String { \
               case lang { \"ja\" -> \"こんにちは\"; \"en\" -> \"hello\"; _ -> \"?\" } }; \
             greet(\"ja\")\n\
             greet(\"en\")\n\
             greet(\"xx\")",
        );
        assert_eq!(out, "こんにちは\nhello\n?\n");
    }

    #[test]
    fn codegen_orパターン() {
        // or パターンが展開され、複数値が同じ腕に当たる。
        let out = compile_and_run(
            "let small = fn(n: Int) -> Bool { case n { 1 | 2 | 3 -> true; _ -> false } }; \
             small(2)\n\
             small(5)",
        );
        assert_eq!(out, "true\nfalse\n");
    }

    #[test]
    fn socket_tcp_connectクライアントが往復する() {
        use std::io::{Read, Write};
        use std::net::TcpListener;
        // Rust 側のエコーサーバ（ephemeral port、1 接続だけ処理）。
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = listener.local_addr().unwrap().port();
        let server = std::thread::spawn(move || {
            if let Ok((mut s, _)) = listener.accept() {
                let mut buf = [0u8; 64];
                if let Ok(n) = s.read(&mut buf) {
                    let _ = s.write_all(&buf[..n]);
                }
            }
        });
        // awa クライアント: connect → write → read → 受信内容を表示。
        let src = format!(
            "print(run(do {{ \
               sock <- tcp_connect(\"127.0.0.1\", {port}); \
               _ <- sock_write(sock, \"ping awa\"); \
               reply <- sock_read(sock); \
               pure(reply) }}))\n"
        );
        let out = compile_and_run(&src);
        let _ = server.join();
        assert_eq!(out, "ping awa\n");
    }

    #[test]
    fn file_async_がネイティブのスレッドプールで動く() {
        // read_file_async はワーカースレッドでブロッキング read し、完了を kqueue 経由で
        // メインループへ通知して Future を解決する。
        let id = std::process::id();
        let path = std::env::temp_dir().join(format!("awa_fa_{}.txt", id));
        let p = path.to_string_lossy().replace('\\', "/");
        std::fs::write(&path, "async file via thread pool").unwrap();
        let out = compile_and_run(&format!(
            "print(run(do {{ s <- read_file_async(\"{p}\"); pure(s) }}))"
        ));
        let _ = std::fs::remove_file(&path);
        assert_eq!(out, "async file via thread pool\n");
    }

    #[test]
    fn socket_echoサーバがネイティブで接続を処理する() {
        use std::io::{Read, Write};
        use std::net::TcpStream;
        let id = std::process::id() as u64 * 1_000_000
            + CODEGEN_CTR.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!("awa_sock_{}", id));
        std::fs::create_dir_all(&dir).unwrap();
        let pf = dir.join("port.txt");
        let pf_s = pf.to_string_lossy().replace('\\', "/");
        // ephemeral port で listen → ポートを書く → 1 接続を accept して echo。
        let src = format!(
            "let server = tcp_listen(0)\n\
             let _ = write_file(\"{pf_s}\", int_to_string(sock_port(server)))\n\
             run(do {{ c <- accept(server); m <- sock_read(c); _ <- sock_write(c, m); pure(0) }})\n"
        );
        let ir = compile_to_llvm(&src).expect("コンパイルが通る");
        let ll = dir.join("echo.ll");
        let rt = dir.join("async_rt.c");
        let exe = dir.join("echo");
        std::fs::write(&ll, &ir).unwrap();
        std::fs::write(&rt, codegen::ASYNC_RUNTIME_C).unwrap();
        let st = std::process::Command::new("clang")
            .arg("-O2").arg(&rt).arg(&ll).arg("-o").arg(&exe).status().expect("clang");
        assert!(st.success(), "clang が失敗");
        let mut child = std::process::Command::new(&exe).spawn().expect("起動");
        // ポートファイルが書かれるまで待つ（並列テストで CPU 飽和しても十分なよう ~15 秒）。
        let mut port = String::new();
        for _ in 0..300 {
            if let Ok(s) = std::fs::read_to_string(&pf) {
                if !s.trim().is_empty() {
                    port = s.trim().to_string();
                    break;
                }
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        let echoed = (|| {
            let port: u16 = port.parse().ok()?;
            // 接続も数回リトライ（サーバプロセスのスケジューリング待ち）。
            let mut stream = None;
            for _ in 0..50 {
                if let Ok(s) = TcpStream::connect(("127.0.0.1", port)) {
                    stream = Some(s);
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            let mut s = stream?;
            s.write_all(b"hello awa socket").ok()?;
            let mut buf = [0u8; 64];
            let n = s.read(&mut buf).ok()?;
            Some(String::from_utf8_lossy(&buf[..n]).to_string())
        })();
        // サーバの終了をポーリングで待つ（接続失敗時も最大 ~15 秒で諦めて kill＝ハングしない）。
        let mut code = None;
        for _ in 0..300 {
            match child.try_wait() {
                Ok(Some(st)) => {
                    code = st.code();
                    break;
                }
                _ => std::thread::sleep(std::time::Duration::from_millis(50)),
            }
        }
        if code.is_none() {
            let _ = child.kill();
            let _ = child.wait();
        }
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(echoed.as_deref(), Some("hello awa socket"));
        // 継続 env も解放されるのでサーバは終了コード 0（リークなし）。
        assert_eq!(code, Some(0), "サーバが正常終了していない");
    }

    // リポジトリの std/ を stdlib ルートに、entry ソースを native ビルドして stdout を返す。
    fn std_build_run(entry_src: &str) -> String {
        let id = std::process::id() as u64 * 1_000_000
            + CODEGEN_CTR.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!("awa_std_{}", id));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("main.awa"), entry_src).unwrap();
        let stdlib = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
        let modules = load_program(&dir, Some(&stdlib), "main").expect("読み込み成功");
        let ir = compile_modules_to_ir(&modules).expect("コンパイル成功");
        let ll = dir.join("out.ll");
        let rt = dir.join("async_rt.c");
        let exe = dir.join("out");
        std::fs::write(&ll, &ir).unwrap();
        std::fs::write(&rt, codegen::ASYNC_RUNTIME_C).unwrap();
        let st = std::process::Command::new("clang")
            .arg("-O2").arg(&rt).arg(&ll).arg("-o").arg(&exe).status().expect("clang");
        assert!(st.success(), "clang が失敗");
        let out = std::process::Command::new(&exe).output().expect("実行");
        let _ = std::fs::remove_dir_all(&dir);
        String::from_utf8(out.stdout).unwrap()
    }

    #[test]
    fn std_io_option_result() {
        let out = std_build_run(
            "import std/io\n\
             import std/option\n\
             import std/result\n\
             io.println(\"from std\")\n\
             option.unwrap_or(option.Some(42), 0)\n\
             result.unwrap_or(result.Err(\"e\"), 5)\n",
        );
        // io.println→"from std"、option.unwrap_or(Some(42))→42、result.unwrap_or(Err)→5。
        assert_eq!(out, "from std\n42\n5\n");
    }

    #[test]
    fn std_string_float() {
        // string.length（UTF-8 文字数）・float.abs/max・string.to_int を native で。
        let out = std_build_run(
            "import std/string\n\
             import std/float\n\
             import std/int\n\
             import std/io\n\
             io.println(int.to_string(string.length(\"héllo\")))\n\
             io.println(float.to_string(float.abs(0.0 - 2.5)))\n\
             io.println(int.to_string(string.to_int(\"42\")))\n",
        );
        // "héllo" は 5 文字（é は 1 コードポイント）、abs(-2.5)=2.5、to_int("42")=42。
        assert_eq!(out, "5\n2.5\n42\n");
    }

    #[test]
    fn std_list_int() {
        // list の map/filter/fold/length/reverse と int.to_string を native で。
        let out = std_build_run(
            "import std/list\n\
             import std/int\n\
             import std/io\n\
             let xs = list.Cons(1, list.Cons(2, list.Cons(3, list.Nil)))\n\
             io.println(int.to_string(list.length(xs)))\n\
             io.println(int.to_string(list.fold(xs, 0, fn(a, x) { a + x })))\n\
             io.println(int.to_string(list.length(list.map(xs, fn(n) { n * 2 }))))\n\
             io.println(int.to_string(int.max(int.abs(-9), 3)))\n",
        );
        // length=3、fold(sum)=6、map の長さ=3、max(abs(-9),3)=9。
        assert_eq!(out, "3\n6\n3\n9\n");
    }

    #[test]
    fn 配列リテラルとプリミティブ_interp() {
        // [..] は不変配列 Array(a) へ脱糖。length/get は O(1)、push/set は永続更新。
        assert_eq!(eval("array_length([1, 2, 3])"), Ok(Value::Int(3)));
        assert_eq!(eval("array_get([10, 20, 30], 2)"), Ok(Value::Int(30)));
        assert_eq!(eval("array_length(array_push([1], 2))"), Ok(Value::Int(2)));
        assert_eq!(eval("array_get(array_set([7, 8], 0, 9), 0)"), Ok(Value::Int(9)));
        // 不変性: 元配列は変わらない。
        assert_eq!(
            eval("let xs = [1, 2]; let _ = array_push(xs, 3); array_length(xs)"),
            Ok(Value::Int(2))
        );
        assert_eq!(eval("array_length([])"), Ok(Value::Int(0)));
    }

    #[test]
    fn 配列パターン_interp() {
        // 固定長パターン。長さがちょうど一致したときだけ成立。
        assert_eq!(eval("case [1, 2] { [a, b] -> a + b; _ -> 0 }"), Ok(Value::Int(3)));
        assert_eq!(eval("case [5] { [x] -> x; _ -> 0 }"), Ok(Value::Int(5)));
        assert_eq!(eval("case [] { [] -> 1; _ -> 0 }"), Ok(Value::Int(1)));
        // 長さ違いは次の腕へ。
        assert_eq!(eval("case [1, 2, 3] { [a, b] -> 0; _ -> 99 }"), Ok(Value::Int(99)));
    }

    #[test]
    fn 配列_native() {
        // ネイティブで配列リテラル・プリミティブ・パターン。終了コード=リーク数=0。
        let (out, code) = compile_run_status(
            "let xs = [1, 2, 3]\n\
             print(\"${array_length(xs)}\")\n\
             print(\"${array_get(xs, 1)}\")\n\
             print(\"${array_length(array_push(xs, 4))}\")\n\
             let describe = fn(a: Array(Int)) {\n\
               case a { [] -> 0; [x] -> x; [p, q] -> p + q; _ -> 99 }\n\
             }\n\
             print(\"${describe([])}\")\n\
             print(\"${describe([5])}\")\n\
             print(\"${describe([3, 4])}\")\n\
             print(\"${describe([1, 2, 3])}\")\n",
        );
        assert_eq!(out, "3\n2\n4\n0\n5\n7\n99\n");
        assert_eq!(code, 0, "リーク検出（生存ヒープ {}）", code);
    }

    #[test]
    fn 配列_文字列要素_native_リーク無し() {
        // ヒープ要素（String）の retain/release バランス。終了コード=リーク数=0。
        let (out, code) = compile_run_status(
            "let xs = [\"a\", \"b\", \"c\"]\n\
             print(array_get(xs, 1))\n\
             let ys = array_push(xs, \"d\")\n\
             print(array_get(ys, 3))\n\
             let zs = array_set(xs, 0, \"Z\")\n\
             print(array_get(zs, 0))\n\
             print(array_get(xs, 0))\n\
             case [\"x\", \"y\"] { [h, t] -> print(h); _ -> print(\"?\") }\n",
        );
        assert_eq!(out, "b\nd\nZ\na\nx\n");
        assert_eq!(code, 0, "リーク検出（生存ヒープ {}）", code);
    }

    #[test]
    fn std_array_native() {
        // std/array の高水準 API（map/filter/fold/at→Option/range/reverse/contains/each）。
        let out = std_build_run(
            "import std/io\n\
             import std/array\n\
             import std/option\n\
             let xs = [1, 2, 3, 4, 5]\n\
             io.println(\"${array.sum(xs)}\")\n\
             io.println(\"${array.sum(array.map(xs, fn(n) { n * 2 }))}\")\n\
             io.println(\"${array.sum(array.filter(xs, fn(n) { n > 2 }))}\")\n\
             io.println(\"${array.get_or(xs, 9, 0 - 1)}\")\n\
             io.println(\"${array.sum(array.range(0, 5))}\")\n\
             io.println(\"${array.get_or(array.reverse(xs), 0, 0)}\")\n\
             io.println(\"${array.contains(xs, 3)}\")\n\
             io.println(\"${array.length(array.concat([1, 2], [3, 4]))}\")\n\
             io.println(\"${option.unwrap_or(array.at(xs, 1), 0)}\")\n",
        );
        // sum=15, map sum=30, filter(>2) sum=12, oob=-1, range sum=10,
        // reverse first=5, contains3=true, concat len=4, at(1)=2。
        assert_eq!(out, "15\n30\n12\n-1\n10\n5\ntrue\n4\n2\n");
    }

    #[test]
    fn std_array_ビルダーで大きい配列() {
        // map/filter/range をビルダー（transient）で O(n) 構築。容量倍増(realloc)も通る。
        // 0..1000 を 2 倍して 500 超だけ残す → 250..999 の偶数 750..1998 の和。
        let out = std_build_run(
            "import std/io\n\
             import std/array\n\
             let xs = array.range(0, 1000)\n\
             let ys = array.map(xs, fn(n) { n * 2 })\n\
             let zs = array.filter(ys, fn(n) { n > 1000 })\n\
             io.println(\"${array.length(xs)} ${array.length(ys)} ${array.length(zs)} ${array.sum(zs)}\")\n",
        );
        // len=1000, mapped=1000, filtered(>1000: 1002..1998 step2 = 499 個), sum。
        // 偶数 1002..1998 の和 = (1002+1998)*499/2 = 748500。
        assert_eq!(out, "1000 1000 499 748500\n");
    }

    #[test]
    fn タプル_interp() {
        // (a, b) はヘテロな組。パターンで分解。型表示は (A, B)。
        assert_eq!(
            eval("case (1, 2) { (a, b) -> a + b }"),
            Ok(Value::Int(3))
        );
        // ネストと混在型。
        assert_eq!(
            eval("case ((1, true), 3) { ((a, _), c) -> a + c }"),
            Ok(Value::Int(4))
        );
        // (e) は単なるグループ化（タプルではない）。
        assert_eq!(eval("(5) + 1"), Ok(Value::Int(6)));
        // タプル値の等価。
        assert_eq!(eval("(1, 2) == (1, 2)"), Ok(Value::Bool(true)));
        assert_eq!(eval("(1, 2) == (1, 3)"), Ok(Value::Bool(false)));
    }

    #[test]
    fn タプル_網羅性() {
        // (Bool, Bool) は要素を尽くせば網羅的（catch-all 不要）。
        let mut checker = Checker::new();
        assert!(run_values(
            "let f = fn(p: (Bool, Bool)) { case p { (true, x) -> x; (false, x) -> x } }\nf((true, false))",
            &mut eval::builtins_env(),
            &mut checker,
        )
        .is_ok());
        // 漏れがあれば NonExhaustiveMatch。
        let mut checker2 = Checker::new();
        let err = run_values(
            "let f = fn(p: (Bool, Bool)) { case p { (true, true) -> 1; (false, false) -> 2 } }\nf((true, true))",
            &mut eval::builtins_env(),
            &mut checker2,
        );
        assert!(matches!(err, Err(AwaError::NonExhaustiveMatch { .. })));
    }

    #[test]
    fn タプル_native() {
        // ネイティブでタプル（ヘテロ・ネスト・型注釈・グループ化）。終了コード=リーク数=0。
        let (out, code) = compile_run_status(
            "let p = (1, \"hi\")\n\
             case p { (n, s) -> print(\"${n} ${s}\") }\n\
             let t = (true, 42, \"x\")\n\
             case t { (b, n, s) -> print(\"${b} ${n} ${s}\") }\n\
             let swap = fn(q: (Int, String)) { case q { (a, b) -> (b, a) } }\n\
             case swap((7, \"seven\")) { (s, n) -> print(\"${s} ${n}\") }\n\
             case ((1, 2), 3) { ((a, b), c) -> print(\"${a} ${b} ${c}\") }\n\
             print(\"${(5) + 1}\")\n",
        );
        assert_eq!(out, "1 hi\ntrue 42 x\nseven 7\n1 2 3\n6\n");
        assert_eq!(code, 0, "リーク検出（生存ヒープ {}）", code);
    }

    #[test]
    fn レコード_interp() {
        // 宣言・生成・フィールドアクセス。
        assert_eq!(
            eval("type P { x: Int\n y: Int }\nlet p = P { x = 3, y = 4 }\np.x + p.y"),
            Ok(Value::Int(7))
        );
        // 更新（不変）: q.y は更新後、p.y は元のまま。
        assert_eq!(
            eval("type P { x: Int\n y: Int }\nlet p = P { x = 1, y = 2 }\nlet q = { p | y = 9 }\nq.y + p.y"),
            Ok(Value::Int(11))
        );
        // パターン分解。
        assert_eq!(
            eval("type P { x: Int\n y: Int }\ncase P { x = 5, y = 6 } { P { x = a, y = b } -> a + b }"),
            Ok(Value::Int(11))
        );
        // 省略形パターン { x, y }。
        assert_eq!(
            eval("type P { x: Int\n y: Int }\ncase P { x = 5, y = 6 } { P { x, y } -> x * y }"),
            Ok(Value::Int(30))
        );
    }

    #[test]
    fn レコード_フィールドエラー() {
        // 未知フィールド。
        assert!(matches!(
            eval("type P { x: Int }\nP { x = 1, z = 2 }"),
            Err(AwaError::UnknownField { .. })
        ));
        // フィールド不足。
        assert!(matches!(
            eval("type P { x: Int\n y: Int }\nP { x = 1 }"),
            Err(AwaError::MissingFields { .. })
        ));
        // 存在しないフィールドアクセス。
        assert!(matches!(
            eval("type P { x: Int }\nlet p = P { x = 1 }\np.z"),
            Err(AwaError::UnknownField { .. })
        ));
    }

    #[test]
    fn レコード_native() {
        // ネイティブでレコード（生成・アクセス・更新・パターン・String フィールド）。リーク 0。
        let (out, code) = compile_run_status(
            "type Point { x: Int\n y: Int }\n\
             type Person { name: String\n age: Int }\n\
             let p = Point { x = 3, y = 4 }\n\
             print(\"${p.x} ${p.y}\")\n\
             let q = { p | y = 10 }\n\
             print(\"${q.y}\")\n\
             case p { Point { x = a, y = b } -> print(\"${a + b}\") }\n\
             let alice = Person { name = \"Alice\", age = 30 }\n\
             let older = { alice | age = 31 }\n\
             print(\"${older.name} ${older.age}\")\n",
        );
        assert_eq!(out, "3 4\n10\n7\nAlice 31\n");
        assert_eq!(code, 0, "リーク検出（生存ヒープ {}）", code);
    }

    #[test]
    fn 構造的等価_native() {
        // 複合型（タプル/配列/レコード/ADT）の ==/!= を native で構造比較。リーク 0。
        let (out, code) = compile_run_status(
            "type P { x: Int\n y: Int }\n\
             type Opt { Some(Int)\n None }\n\
             print(\"${(1, 2) == (1, 2)}\")\n\
             print(\"${(1, \"a\") == (1, \"b\")}\")\n\
             print(\"${[1, 2, 3] == [1, 2, 3]}\")\n\
             print(\"${[1, 2] == [1, 2, 3]}\")\n\
             print(\"${P { x = 1, y = 2 } == P { x = 1, y = 2 }}\")\n\
             print(\"${Some(5) == Some(6)}\")\n\
             print(\"${Some(5) == None}\")\n\
             print(\"${(1, [2, 3]) != (1, [2, 3])}\")\n",
        );
        assert_eq!(out, "true\nfalse\ntrue\nfalse\ntrue\nfalse\nfalse\nfalse\n");
        assert_eq!(code, 0, "リーク検出（生存ヒープ {}）", code);
    }

    #[test]
    fn 構造的等価_interp() {
        // インタプリタの構造比較（ネイティブの eq$ty と同じ意味論）。
        assert_eq!(eval("(1, [2, 3]) == (1, [2, 3])"), Ok(Value::Bool(true)));
        assert_eq!(eval("(1, [2, 3]) == (1, [2, 4])"), Ok(Value::Bool(false)));
        // ネスト ADT（tag＋フィールド再帰）。
        assert_eq!(eval("type T { A(Int)\n B }\nA(1) == A(1)"), Ok(Value::Bool(true)));
        assert_eq!(eval("type T { A(Int)\n B }\nA(1) == B"), Ok(Value::Bool(false)));
    }

    #[test]
    fn std_option_do記法() {
        // do は .flatMap 連鎖へ脱糖し、Option の pub impl メソッドで解決される。
        let out = std_build_run(
            "import std/io\n\
             import std/option\n\
             let added = do {\n\
               x <- option.Some(10)\n\
               y <- option.Some(20)\n\
               option.Some(x + y)\n\
             }\n\
             let short = do {\n\
               x <- option.Some(1)\n\
               _ <- option.None\n\
               option.Some(x)\n\
             }\n\
             io.println(\"added=${option.unwrap_or(added, 0)}\")\n\
             io.println(\"short=${option.unwrap_or(short, 99)}\")\n",
        );
        // 全 Some なら 10+20=30、途中 None で短絡 → unwrap_or の既定 99。
        assert_eq!(out, "added=30\nshort=99\n");
    }

    #[test]
    fn std_result_do記法() {
        let out = std_build_run(
            "import std/io\n\
             import std/result\n\
             let safe_div = fn(a: Int, b: Int) -> result.Result(Int, String) {\n\
               case b == 0 { true -> result.Err(\"div0\"); false -> result.Ok(a / b) }\n\
             }\n\
             let ok = do {\n\
               x <- safe_div(100, 5)\n\
               y <- safe_div(x, 2)\n\
               result.Ok(y)\n\
             }\n\
             let bad = do {\n\
               x <- safe_div(100, 0)\n\
               result.Ok(x)\n\
             }\n\
             io.println(\"ok=${result.unwrap_or(ok, 0 - 1)}\")\n\
             io.println(\"bad=${result.unwrap_or(bad, 0 - 1)}\")\n",
        );
        // 100/5=20, 20/2=10 → ok=10。Err で短絡 → bad は既定 -1。
        assert_eq!(out, "ok=10\nbad=-1\n");
    }

    #[test]
    fn std_list_any_all_find_take_drop() {
        let out = std_build_run(
            "import std/io\n\
             import std/option\n\
             import std/list\n\
             let xs = list.Cons(1, list.Cons(2, list.Cons(3, list.Nil)))\n\
             io.println(\"any=${list.any(xs, fn(n) { n > 2 })}\")\n\
             io.println(\"all=${list.all(xs, fn(n) { n > 0 })}\")\n\
             io.println(\"find=${option.unwrap_or(list.find(xs, fn(n) { n > 1 }), 0)}\")\n\
             io.println(\"take=${list.length(list.take(xs, 2))}\")\n\
             io.println(\"drop=${list.length(list.drop(xs, 2))}\")\n",
        );
        assert_eq!(out, "any=true\nall=true\nfind=2\ntake=2\ndrop=1\n");
    }

    #[test]
    fn std_list_コレクション拡充() {
        let out = std_build_run(
            "import std/io\n\
             import std/list\n\
             import std/option\n\
             let ns = list.range(1, 6)\n\
             io.println(\"${list.sum(ns)}\")\n\
             io.println(\"${list.contains(ns, 3)}\")\n\
             io.println(\"${list.length(list.flat_map(ns, fn(n) { list.range(0, n) }))}\")\n\
             let s = list.sort(list.Cons(3, list.Cons(1, list.Cons(2, list.Nil))), fn(a, b) { a < b })\n\
             io.println(\"${option.unwrap_or(list.head(s), 0)}${option.unwrap_or(list.last(s), 0)}\")\n\
             io.println(\"${list.length(list.take_while(ns, fn(n) { n < 3 }))}\")\n",
        );
        // sum=15, contains=true, flat_map 長さ=15, sort 先頭1末尾3→13, take_while=2。
        assert_eq!(out, "15\ntrue\n15\n13\n2\n");
    }

    #[test]
    fn std_math_関数() {
        let out = std_build_run(
            "import std/io\n\
             import std/math\n\
             import std/float\n\
             io.println(float.to_string(math.sqrt(2.0)))\n\
             io.println(float.to_string(math.pow(2.0, 10.0)))\n\
             io.println(float.to_string(math.floor(3.7)))\n\
             io.println(float.to_string(math.ln(math.e)))\n",
        );
        assert_eq!(out, "1.4142135623730951\n1024\n3\n1\n");
    }

    #[test]
    fn std_math_拡張関数() {
        let out = std_build_run(
            "import std/io\n\
             import std/math\n\
             import std/float\n\
             import std/int\n\
             io.println(float.to_string(math.cbrt(27.0)))\n\
             io.println(float.to_string(math.hypot(3.0, 4.0)))\n\
             io.println(float.to_string(math.log2(8.0)))\n\
             io.println(float.to_string(math.round(math.degrees(math.pi))))\n\
             io.println(\"${int.gcd(12, 18)} ${int.lcm(4, 6)} ${int.factorial(5)}\")\n",
        );
        assert_eq!(out, "3\n5\n3\n180\n6 12 120\n");
    }

    #[test]
    fn codegen_floatは最短往復表現で表示される() {
        // native の float 表示が interpreter（Rust）と一致する。
        let (out, code) = compile_run_status(
            "print(float_to_string(0.1))\nprint(float_to_string(1.0 / 3.0))",
        );
        assert_eq!(out, "0.1\n0.3333333333333333\n");
        assert_eq!(code, 0);
    }

    #[test]
    fn std_int_float_string_追加関数() {
        let out = std_build_run(
            "import std/io\n\
             import std/int\n\
             import std/string\n\
             io.println(\"${int.clamp(15, 0, 10)} ${int.pow(2, 10)} ${int.sign(-3)}\")\n\
             io.println(\"${int.is_even(4)}\")\n\
             io.println(\"[${string.trim(\"  hi  \")}]\")\n\
             io.println(string.repeat(\"ab\", 3))\n\
             io.println(string.reverse(\"abcあ\"))\n",
        );
        assert_eq!(out, "10 1024 -1\ntrue\n[hi]\nababab\nあcba\n");
    }

    #[test]
    fn std_option_result_コンビネータ() {
        let out = std_build_run(
            "import std/io\n\
             import std/option\n\
             import std/result\n\
             let s = option.Some(5)\n\
             io.println(\"${option.is_some(s)}\")\n\
             io.println(\"${option.unwrap_or(option.or(option.None, option.Some(9)), 0)}\")\n\
             io.println(\"${option.unwrap_or(option.flatten(option.Some(option.Some(7))), 0)}\")\n\
             io.println(\"${result.unwrap_or(option.to_result(option.None, \"e\"), 0)}\")\n\
             io.println(\"${result.is_err(result.Err(\"boom\"))}\")\n\
             io.println(\"${result.unwrap_or_else(result.Err(\"boom\"), fn(m) { string_length(m) })}\")\n",
        );
        // is_some=true, or→9, flatten→7, None→Result Err→既定0, is_err=true, unwrap_or_else→len(\"boom\")=4。
        assert_eq!(out, "true\n9\n7\n0\ntrue\n4\n");
    }

    #[test]
    fn std_dict_辞書() {
        let out = std_build_run(
            "import std/io\n\
             import std/dict\n\
             import std/option\n\
             let d = dict.insert(dict.insert(dict.new(), \"a\", 1), \"b\", 2)\n\
             io.println(\"${option.unwrap_or(dict.get(d, \"a\"), 0)}\")\n\
             io.println(\"${dict.size(d)}\")\n\
             io.println(\"${dict.has_key(d, \"c\")}\")\n\
             io.println(\"${dict.size(dict.remove(d, \"a\"))}\")\n\
             io.println(\"${option.unwrap_or(dict.get(dict.insert(d, \"a\", 9), \"a\"), 0)}\")\n",
        );
        // get a=1, size=2, has c=false, remove後=1, 上書き a=9。
        assert_eq!(out, "1\n2\nfalse\n1\n9\n");
    }

    #[test]
    fn std_json_parse_stringify_roundtrip() {
        // パース→再シリアライズが入力と一致（数値/配列/入れ子/true/null）。
        let out = std_build_run(
            "import std/io\n\
             import std/json\n\
             import std/result\n\
             let src = \"[1,2,[3,true],null]\"\n\
             let r = case json.parse(src) {\n\
               result.Ok(j) -> json.stringify(j)\n\
               result.Err(e) -> e\n\
             }\n\
             io.println(r)\n",
        );
        assert_eq!(out, "[1,2,[3,true],null]\n");
    }

    #[test]
    fn std_json_unicode_エスケープ() {
        // \uXXXX（BMP）をコードポイントへ。café / あ。
        let out = std_build_run(
            "import std/io\n\
             import std/json\n\
             import std/result\n\
             io.println(case json.parse(\"[\\\"caf\\\\u00e9\\\",\\\"\\\\u3042\\\"]\") {\n\
               result.Ok(j) -> json.stringify(j)\n\
               result.Err(e) -> e\n\
             })\n",
        );
        assert_eq!(out, "[\"café\",\"あ\"]\n");
    }

    #[test]
    fn std_json_object_と_エラー() {
        let out = std_build_run(
            "import std/io\n\
             import std/json\n\
             import std/result\n\
             io.println(case json.parse(\"{}\") { result.Ok(j) -> json.stringify(j); result.Err(e) -> e })\n\
             io.println(case json.parse(\"[1,\") { result.Ok(j) -> \"ok\"; result.Err(e) -> \"err\" })\n",
        );
        // 空オブジェクトは {} に往復、途中で切れた配列はエラー。
        assert_eq!(out, "{}\nerr\n");
    }

    #[test]
    fn 文字列_バイト長と大小変換() {
        // UTF-8: "あ" は 1 文字・3 バイト。
        assert_eq!(eval(r#"string_byte_length("あ")"#), Ok(Value::Int(3)));
        assert_eq!(eval(r#"string_length("あ")"#), Ok(Value::Int(1)));
    }

    #[test]
    fn std_string_lower_upper() {
        let out = std_build_run(
            "import std/io\n\
             import std/string\n\
             io.println(string.to_lower(\"Hello, WORLD! 日本\"))\n\
             io.println(string.to_upper(\"Hello, world!\"))\n\
             io.println(\"${string.byte_length(\"あ\")}\")\n",
        );
        assert_eq!(out, "hello, world! 日本\nHELLO, WORLD!\n3\n");
    }

    #[test]
    fn std_http_大小無視ヘッダと任意メソッド() {
        let out = std_build_run(
            "import std/io\n\
             import std/http\n\
             import std/list\n\
             import std/option\n\
             import std/result\n\
             let raw = \"HTTP/1.1 201 Created\\r\\nCONTENT-TYPE: application/json\\r\\n\\r\\n{}\"\n\
             io.println(case http.parse_response(raw) {\n\
               result.Ok(r) -> case http.get_header(http.resp_headers(r), \"content-type\") {\n\
                 option.Some(v) -> \"${http.resp_status(r)}:${v}\"\n\
                 option.None -> \"no-ctype\"\n\
               }\n\
               result.Err(e) -> e\n\
             })\n\
             io.println(http.build_request(\"POST\", \"h\", \"/p\", list.Cons(http.header(\"X-A\", \"1\"), list.Nil), \"body\"))\n",
        );
        // 大文字ヘッダを小文字名で引ける／build_request は Content-Length=4（バイト）。
        assert!(out.contains("201:application/json\n"), "got: {out}");
        assert!(out.contains("POST /p HTTP/1.1\r\n"), "got: {out:?}");
        assert!(out.contains("X-A: 1\r\n"));
        assert!(out.contains("Content-Length: 4\r\n"));
    }

    #[test]
    fn std_http_chunked_デコード() {
        let out = std_build_run(
            "import std/io\n\
             import std/http\n\
             import std/result\n\
             let raw = \"HTTP/1.1 200 OK\\r\\nTransfer-Encoding: chunked\\r\\n\\r\\n4\\r\\nWiki\\r\\n5\\r\\npedia\\r\\n0\\r\\n\\r\\n\"\n\
             io.println(case http.parse_response(raw) {\n\
               result.Ok(r) -> http.resp_body(r)\n\
               result.Err(e) -> e\n\
             })\n",
        );
        assert_eq!(out, "Wikipedia\n");
    }

    #[test]
    fn std_http_parse_and_build() {
        // ソケットを使わない純粋関数（parse_response / parse_request / response_to_string）を検証。
        let out = std_build_run(
            "import std/io\n\
             import std/http\n\
             import std/result\n\
             let raw = \"HTTP/1.1 200 OK\\r\\nContent-Type: text/plain\\r\\n\\r\\nhello\"\n\
             io.println(case http.parse_response(raw) {\n\
               result.Ok(r) -> \"${http.resp_status(r)}:${http.resp_body(r)}\"\n\
               result.Err(e) -> e\n\
             })\n\
             let req = \"POST /submit HTTP/1.1\\r\\nHost: x\\r\\n\\r\\nbody\"\n\
             io.println(case http.parse_request(req) {\n\
               result.Ok(q) -> \"${http.req_method(q)} ${http.req_path(q)} ${http.req_body(q)}\"\n\
               result.Err(e) -> e\n\
             })\n\
             io.println(case http.parse_response(http.response_to_string(http.response(201, \"hi\"))) {\n\
               result.Ok(r) -> \"${http.resp_status(r)}:${http.resp_body(r)}\"\n\
               result.Err(e) -> e\n\
             })\n",
        );
        // レスポンス解析 / リクエスト解析 / 組み立て→解析の往復。
        assert_eq!(out, "200:hello\nPOST /submit body\n201:hi\n");
    }

    #[test]
    fn 文字列はバックスラッシュrでcrになる() {
        // \r エスケープ（HTTP の CRLF で必要）。
        assert_eq!(eval(r#""a\r\nb""#), Ok(Value::Str("a\r\nb".to_string())));
    }

    #[test]
    fn 焼き込み標準ライブラリは_stdlibパスなしで解決できる() {
        // stdlib=None でも `import std/...` がバイナリ焼き込みソースから解決される
        // （cwd・環境変数に非依存）。エントリは std/ を持たない一時ディレクトリに置く。
        let id = std::process::id() as u64 * 1_000_000
            + CODEGEN_CTR.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!("awa_embed_{}", id));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("main.awa"),
            "import std/string\nstring.length(\"awa\")\n",
        )
        .unwrap();
        // stdlib パスを渡さない＝焼き込みのみで解決する。
        let modules = load_program(&dir, None, "main").expect("焼き込みから解決できる");
        let typed = check_program_modules(&modules).expect("型検査成功");
        let _ = std::fs::remove_dir_all(&dir);
        // std/string と main の 2 モジュールが読めていること。
        assert!(modules.iter().any(|m| m.path == vec!["std".to_string(), "string".to_string()]));
        assert!(!typed.is_empty());
    }

    #[test]
    fn codegen_論理演算子と基数リテラル() {
        // 0xFF + 0b1 = 256、|| の短絡。
        let (out, code) = compile_run_status(
            "print(int_to_string(0xFF + 0b1))\nprint(\"${false || true}\")",
        );
        assert_eq!(out, "256\ntrue\n");
        assert_eq!(code, 0);
    }

    #[test]
    fn codegen_短絡は右辺を評価しない() {
        // false && (0除算) が走ると exit 1 になるが、短絡するので 0 で正常終了。
        let (out, code) = compile_run_status("print(\"${false && 1 / 0 == 0}\")");
        assert_eq!(out, "false\n");
        assert_eq!(code, 0);
    }

    #[test]
    fn codegen_末尾再帰は定数スタック_tco() {
        // -O2 の sibling-call 最適化で 100 万段の末尾再帰が定数スタックで回る。
        let (out, code) = compile_run_status(
            "let c = fn(n: Int, a: Int) -> Int { case n == 0 { true -> a; false -> c(n - 1, a + 1) } }\n\
             print(int_to_string(c(1000000, 0)))",
        );
        assert_eq!(out, "1000000\n");
        assert_eq!(code, 0);
    }

    #[test]
    fn codegen_main関数が入口になる() {
        // main を定義するとそれが入口。戻り値は表示されず（出力なし）、RC も釣り合う（終了コード 0）。
        let (out, code) = compile_run_status("let main = fn() -> Int { 1 + 2 }");
        assert_eq!(out, "");
        assert_eq!(code, 0);
    }

    #[test]
    fn codegen_main_はヒープ値を返してもリークしない() {
        // main の戻り値（String=ヒープ）が release され、生存数 0 で終了する。
        let (out, code) = compile_run_status("let main = fn() -> String { \"hi\" <> \"!\" }");
        assert_eq!(out, "");
        assert_eq!(code, 0);
    }

    #[test]
    fn main_と裸のトップレベル式の混在はエラー() {
        let id = std::process::id() as u64 * 1_000_000
            + CODEGEN_CTR.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!("awa_entry_{}", id));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("main.awa"), "let main = fn() { 1 }\n2\n").unwrap();
        let modules = load_program(&dir, None, "main").unwrap();
        let err = check_program_modules(&modules).unwrap_err();
        let _ = std::fs::remove_dir_all(&dir);
        assert!(err.contains("main") && err.contains("トップレベル"), "got: {err}");
    }

    #[test]
    fn codegen_io_がネイティブ実行される() {
        // write_file→read_file→print が native で動き、RC が釣り合って終了コード 0。
        let id = std::process::id();
        let path = std::env::temp_dir().join(format!("awa_native_io_{}.txt", id));
        let p = path.to_string_lossy().replace('\\', "/");
        let (out, code) = compile_run_status(&format!(
            "let _ = write_file(\"{p}\", \"data via awa\")\nprint(read_file(\"{p}\"))"
        ));
        let _ = std::fs::remove_file(&path);
        assert_eq!(out, "data via awa\n");
        assert_eq!(code, 0, "リークしている（残存 {}）", code);
    }

    #[test]
    fn codegen_async_がネイティブ実行される() {
        // do/Future が C ランタイムのイベントループ上で native 実行される（インタプリタと一致）。
        // 継続 env もランタイムが release するので終了コード 0（リークなし）。
        let (out, code) = compile_run_status(
            "run(do { x <- delay(10, 1); y <- delay(5, 2); pure(x + y) })\n\
             run(delay(3, 20).map(fn(n: Int) -> Int { n + 1 }))",
        );
        assert_eq!(out, "3\n21\n");
        assert_eq!(code, 0, "継続 env がリークしている（残存 {}）", code);
    }

    #[test]
    fn codegen_ゼロ除算は実行時に検出() {
        // 整数の 0 除算は実行時チェックで abort（終了コード 1）。
        let ir = compile_to_llvm("let div = fn(a: Int, b: Int) -> Int { a / b }; div(10, 0)")
            .expect("コンパイルは通る");
        let id = std::process::id() as u64 * 1_000_000
            + CODEGEN_CTR.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir();
        let ll = dir.join(format!("awa_cg_{}.ll", id));
        let rt = dir.join(format!("awa_rt_{}.c", id));
        let exe = dir.join(format!("awa_cg_{}", id));
        std::fs::write(&ll, &ir).unwrap();
        std::fs::write(&rt, codegen::ASYNC_RUNTIME_C).unwrap();
        let st = std::process::Command::new("clang")
            .arg("-O2").arg(&rt).arg(&ll).arg("-o").arg(&exe).status().expect("clang を起動できる");
        assert!(st.success(), "clang が失敗");
        let out = std::process::Command::new(&exe).output().expect("実行できる");
        let _ = std::fs::remove_file(&ll);
        let _ = std::fs::remove_file(&exe);
        assert!(!out.status.success(), "0 除算は異常終了するはず");
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(stdout.contains("division by zero"), "メッセージ出力: {}", stdout);
    }

    #[test]
    fn example_native() {
        let src = include_str!("../../../examples/native.awa");
        let out = compile_and_run(src);
        assert_eq!(out, "7\n120\n55\ntrue\n");
    }

    #[test]
    fn doの中でletが書ける() {
        // let z = x * 10 を挟む。Some(2) → z=20 → Some(22)。
        match eval(&format!(
            "{MOPT} do {{ x <- Some(2); let z = x * 10; Some(z + 2) }}"
        )) {
            Ok(Value::Data { ctor, args }) => {
                assert_eq!(ctor, "Some");
                assert_eq!(args, vec![num(22.0)]);
            }
            other => panic!("Some(22) のはず: {:?}", other),
        }
    }
}
