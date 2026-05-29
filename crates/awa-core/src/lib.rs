
pub mod ast;
pub mod codegen;
pub mod error;
pub mod eval;
pub mod infer;
pub mod lexer;
pub mod match_check;
pub mod parser;
pub mod query;
pub mod resolve;
pub mod span;
pub mod token;
pub mod types;
pub mod value;

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use ast::TStmt;
use error::{LocatedError, report};
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
// `import std/...` は stdlib から、それ以外は root から解決する。
pub fn load_program(
    root: &PathBuf,
    stdlib: Option<&Path>,
    entry_path: &str,
) -> Result<Vec<LoadedModule>, String> {
    // エントリのモジュールパス: ".awa" を外して "/" で分割。
    let rel = entry_path.strip_suffix(".awa").unwrap_or(entry_path);
    let entry_segs: Vec<String> = rel.split('/').map(|s| s.to_string()).collect();
    let mut loaded: Vec<LoadedModule> = Vec::new();
    let mut done: HashSet<String> = HashSet::new();
    let mut in_progress: HashSet<String> = HashSet::new();
    load_module(root, stdlib, &entry_segs, true, &mut loaded, &mut done, &mut in_progress)?;
    Ok(loaded)
}

fn load_module(
    root: &PathBuf,
    stdlib: Option<&Path>,
    segs: &[String],
    is_entry: bool,
    loaded: &mut Vec<LoadedModule>,
    done: &mut HashSet<String>,
    in_progress: &mut HashSet<String>,
) -> Result<(), String> {
    let key = segs.join("/");
    if done.contains(&key) {
        return Ok(()); // 既に読み込み済み（共有 import）
    }
    if !in_progress.insert(key.clone()) {
        return Err(format!("import が循環しています: {}", key));
    }
    // `std/...` は stdlib ルートから、それ以外はプロジェクトルートから解決する。
    let is_std = segs.first().map(|s| s.as_str()) == Some("std");
    let mut path = match (is_std, stdlib) {
        (true, Some(s)) => s.to_path_buf(),
        _ => root.clone(),
    };
    for s in segs {
        path.push(s);
    }
    path.set_extension("awa");
    let source = std::fs::read_to_string(&path)
        .map_err(|e| format!("モジュールを読めません ({}): {}", path.display(), e))?;
    // 構文解析（このファイルのソースでエラー位置を出す）。
    let parsed = (|| {
        let tokens = lexer::tokenize(&source)?;
        Parser::new(tokens).parse_module()
    })()
    .map_err(|e| report(&e, &source))?;
    // 依存先を先に読み込む（post-order で依存順に並ぶ）。
    for imp in &parsed.imports {
        load_module(root, stdlib, &imp.path, false, loaded, done, in_progress)?;
    }
    in_progress.remove(&key);
    done.insert(key);
    // エントリは接頭辞なし（既存の単一ファイル挙動・@main を維持）、他は "seg.seg." で修飾。
    let prefix = if is_entry {
        String::new()
    } else {
        format!("{}.", segs.join("."))
    };
    loaded.push(LoadedModule {
        path: segs.to_vec(),
        prefix,
        imports: parsed.imports,
        exports: parsed.exports,
        stmts: parsed.stmts,
        source,
    });
    Ok(())
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
    Ok(typed)
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
    fn if_は選ばれた腕だけを評価する() {
        assert_eq!(eval("let x = 0; if x == 0 { 100 } else { 1 / x }"), Ok(num(100.0)));
    }

    #[test]
    fn 比較は加減算より弱く結合する() {
        assert_eq!(eval("1 + 2 == 3"), Ok(Value::Bool(true)));
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
    fn 型エラー_ifの条件が数値() {
        assert_eq!(
            eval("if 1 { 2 } else { 3 }"),
            Err(AwaError::CannotUnify { left: "Int".to_string(), right: "Bool".to_string() })
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
            eval("let fact = fn(n) { if n == 0 { 1 } else { n * fact(n - 1) } }; fact(5)"),
            Ok(num(120.0))
        );
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
            .arg(&rt).arg(&ll).arg("-o").arg(&exe).status().expect("clang を起動できる");
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
            .arg(&rt).arg(&ll).arg("-o").arg(&exe).status().expect("clang を起動できる");
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
            "let fact = fn(n: Int) -> Int { if n == 0 { 1 } else { n * fact(n - 1) } }; fact(5)",
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
            .arg(&rt).arg(&ll).arg("-o").arg(&exe).status().expect("clang");
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
            .arg(&rt).arg(&ll).arg("-o").arg(&exe).status().expect("clang");
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
            .arg(&rt).arg(&ll).arg("-o").arg(&exe).status().expect("clang を起動できる");
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
