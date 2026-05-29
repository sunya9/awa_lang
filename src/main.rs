use std::io::{Write, stdout};

mod ast;
mod codegen;
mod error;
mod eval;
mod infer;
mod lexer;
mod match_check;
mod parser;
mod span;
mod token;
mod types;
mod value;

use ast::TStmt;
use error::{LocatedError, report};
use eval::eval_program;
use infer::Checker;
use parser::Parser;
use types::{Type, render_type};
use value::{Env, Value};

// ===== パイプライン =====
// tokenize → parse → type-check → eval を ? で連結。どこで失敗しても Err が伝わる。
// 型検査（checker.check_program）を eval の前に挟むのが Phase 2 の要。型エラーは
// 実行前に捕まり、eval には well-typed なプログラムだけが進む。
// env / checker をともに借用で受け取るので、ファイル実行（使い捨て）も
// REPL（行をまたいで持ち越す）もこの 1 つの関数で賄える。
// 各文の (値, 型) を順に返す。型検査は型付き AST（TStmt）を返し、eval はそれを実行する。
// 型宣言は値文を生まないので、値と型はどちらも Let/Expr 文だけから同順で取れる。
fn run(
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

// ===== ファイル実行 =====
// .awa ファイルを読んで一括実行する。エラーが出たら表示して終了する
// （REPL と違い、続行はしない）。
fn run_file(path: &str) {
    let source = match std::fs::read_to_string(path) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("ファイルを読めません ({}): {}", path, e);
            std::process::exit(1);
        }
    };
    let mut env = eval::builtins_env();
    let mut checker = Checker::new();
    match run(&source, &mut env, &mut checker) {
        Ok(rows) => {
            for (v, t) in rows {
                println!("{} : {}", v, render_type(&t));
            }
        }
        Err(e) => {
            eprintln!("{}", report(&e, &source));
            std::process::exit(1);
        }
    }
}

// ===== REPL（対話実行）=====
// 1 行読んでは評価する。env はループの外で持つので、let で束縛した変数を
// 次の行から参照できる（セッション状態）。run がエラーを返しても落とさず、
// 表示して次の入力へ進む（ファイル実行とは逆の方針）。
fn repl() -> std::io::Result<()> {
    println!("awa REPL — Ctrl-D で終了");
    let mut env = eval::builtins_env();
    let mut checker = Checker::new();
    let stdin = std::io::stdin();
    loop {
        print!("awa> ");
        stdout().flush()?;
        let mut line = String::new();
        let n = stdin.read_line(&mut line);
        if n? == 0 {
            break; // EOF
        }
        match run(&line, &mut env, &mut checker) {
            Ok(rows) => {
                for (v, t) in rows {
                    println!("{} : {}", v, render_type(&t));
                }
            }
            Err(e) => eprintln!("{}", report(&e, &line)),
        }
    }
    Ok(())
}

// ソースを型検査し、型付きプログラムを LLVM IR テキストへコンパイルする。
fn compile_to_llvm(source: &str) -> Result<String, LocatedError> {
    let tokens = lexer::tokenize(source)?;
    let program = Parser::new(tokens).parse_program()?;
    let mut checker = Checker::new();
    let typed = checker.check_program(&program)?;
    Ok(codegen::compile_program(&typed)?)
}

// `awa emit <file>`: LLVM IR を標準出力へ。
fn emit_file(path: &str) {
    let source = std::fs::read_to_string(path).unwrap_or_else(|e| {
        eprintln!("ファイルを読めません ({}): {}", path, e);
        std::process::exit(1);
    });
    match compile_to_llvm(&source) {
        Ok(ir) => print!("{}", ir),
        Err(e) => {
            eprintln!("{}", report(&e, &source));
            std::process::exit(1);
        }
    }
}

// `awa build <file>`: IR を生成し clang でネイティブ実行ファイルを作る。
fn build_file(path: &str) {
    let source = std::fs::read_to_string(path).unwrap_or_else(|e| {
        eprintln!("ファイルを読めません ({}): {}", path, e);
        std::process::exit(1);
    });
    let ir = match compile_to_llvm(&source) {
        Ok(ir) => ir,
        Err(e) => {
            eprintln!("{}", report(&e, &source));
            std::process::exit(1);
        }
    };
    let base = path.strip_suffix(".awa").unwrap_or(path);
    let ll_path = format!("{}.ll", base);
    if let Err(e) = std::fs::write(&ll_path, &ir) {
        eprintln!("IR を書けません ({}): {}", ll_path, e);
        std::process::exit(1);
    }
    let status = std::process::Command::new("clang")
        .args([&ll_path, "-o", base])
        .status();
    match status {
        Ok(s) if s.success() => println!("built: {}", base),
        Ok(s) => {
            eprintln!("clang が失敗しました: {}", s);
            std::process::exit(1);
        }
        Err(e) => {
            eprintln!("clang を起動できません: {}", e);
            std::process::exit(1);
        }
    }
}

fn main() {
    // 引数: awa emit/build <file> はコンパイル、awa <file> はインタプリタ実行、引数なしは REPL。
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(|s| s.as_str()) {
        Some("emit") | Some("build") => {
            let Some(path) = args.get(2) else {
                eprintln!("使い方: awa {} <file.awa>", &args[1]);
                std::process::exit(1);
            };
            if args[1] == "emit" {
                emit_file(path);
            } else {
                build_file(path);
            }
        }
        Some(path) => run_file(path),
        None => {
            if let Err(e) = repl() {
                eprintln!("REPL エラー: {}", e);
                std::process::exit(1);
            }
        }
    }
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
        let src = include_str!("../examples/polymorphism.awa");
        let mut env = eval::builtins_env();
        let mut checker = Checker::new();
        let rows = run(src, &mut env, &mut checker).expect("型検査・評価が通るはず");
        // 末尾は fact(6) = 720 : Num。
        assert_eq!(rows.last().map(|(v, _)| v.clone()), Some(num(720.0)));
    }

    #[test]
    fn example_adt() {
        let src = include_str!("../examples/adt.awa");
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
        let src = include_str!("../examples/annotations.awa");
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
        let src = include_str!("../examples/pipe.awa");
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
        let src = include_str!("../examples/methods.awa");
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
        let src = include_str!("../examples/do.awa");
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
        let src = include_str!("../examples/types.awa");
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
        let exe = dir.join(format!("awa_cg_{}", id));
        std::fs::write(&ll, &ir).unwrap();
        let st = std::process::Command::new("clang")
            .arg(&ll)
            .arg("-o")
            .arg(&exe)
            .status()
            .expect("clang を起動できる");
        assert!(st.success(), "clang が失敗");
        let out = std::process::Command::new(&exe).output().expect("実行できる");
        let _ = std::fs::remove_file(&ll);
        let _ = std::fs::remove_file(&exe);
        String::from_utf8(out.stdout).unwrap()
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
    fn example_native() {
        let src = include_str!("../examples/native.awa");
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
