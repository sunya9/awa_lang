use std::collections::HashMap;
use std::io::{Write, stdout};

mod ast;
mod error;
mod eval;
mod lexer;
mod parser;
mod token;
mod types;
mod value;

use error::AwaError;
use eval::eval_program;
use parser::Parser;
use value::{Env, Value};

// ===== パイプライン =====
// tokenize → parse → eval を ? で連結。どこで失敗しても Err が呼び出し側へ伝わる。
// env を借用で受け取るので、ファイル実行（使い捨て env）も REPL（持ち越す env）も
// この 1 つの関数で賄える。
fn run(source: &str, env: &mut Env) -> Result<Vec<Value>, AwaError> {
    let tokens = lexer::tokenize(source)?;
    let mut parser = Parser::new(tokens);
    let program = parser.parse_program()?;
    eval_program(&program, env)
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
    let mut env: Env = HashMap::new();
    match run(&source, &mut env) {
        Ok(values) => {
            for v in values {
                println!("{}", v);
            }
        }
        Err(e) => {
            eprintln!("error: {}", e);
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
    let mut env: Env = HashMap::new();
    let stdin = std::io::stdin();
    loop {
        print!("awa> ");
        stdout().flush()?;
        let mut line = String::new();
        let n = stdin.read_line(&mut line);
        if n? == 0 {
            break; // EOF
        }
        match run(&line, &mut env) {
            Ok(values) => {
                for v in values {
                    println!("{}", v);
                }
            }
            Err(e) => eprintln!("エラー: {}", e),
        }
    }
    Ok(())
}

fn main() {
    // 引数: awa <file> ならファイル実行、引数なしなら REPL。
    // args().nth(1) は最初の引数（args[0] は実行ファイル名なので飛ばす）。
    match std::env::args().nth(1) {
        Some(path) => run_file(&path),
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

    fn eval(src: &str) -> Result<Value, AwaError> {
        let mut env = Env::new();
        run(src, &mut env).map(|values| values.last().cloned().unwrap_or(Value::Unit))
    }

    fn num(n: f64) -> Value {
        Value::Num(n)
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

    #[test]
    fn 型エラー_boolに算術() {
        assert_eq!(
            eval("true + 1"),
            Err(AwaError::TypeError { expected: "Num", found: "Bool" })
        );
    }

    #[test]
    fn 型エラー_ifの条件が数値() {
        assert_eq!(
            eval("if 1 { 2 } else { 3 }"),
            Err(AwaError::TypeError { expected: "Bool", found: "Num" })
        );
    }

    #[test]
    fn 型エラー_numとboolの比較() {
        assert_eq!(
            eval("1 == true"),
            Err(AwaError::TypeError { expected: "Num", found: "Bool" })
        );
    }

    #[test]
    fn repl_env持ち越し() {
        let mut env = Env::new();
        assert_eq!(run("let x = 10", &mut env), Ok(vec![Value::Num(10.0)]));
        assert_eq!(run("x + 5", &mut env), Ok(vec![Value::Num(15.0)]));
    }

    #[test]
    fn 括弧の対応が壊れた入力() {
        // (1 + 2 ; → ) を期待した位置で終端（;）が来る
        assert_eq!(eval("(1 + 2;"), Err(AwaError::UnexpectedToken("Terminator".to_string())));
    }

    #[test]
    fn 基本的な関数呼び出し() {
        assert_eq!(eval("let add = fn(a, b) { a + b }; add(1, 2)"), Ok(num(3.0)));
    }

    #[test]
    fn クロージャは定義時の変数を捕捉する() {
        assert_eq!(
            eval("let make_adder = fn(n) { fn(x) { x + n } }; let add5 = make_adder(5); add5(10)"),
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
            eval("let add = fn(a, b) { a + b }; add(1)"),
            Err(AwaError::ArityMismatch { expected: 2, found: 1 })
        );
    }

    #[test]
    fn 関数でない値の呼び出しはエラー() {
        assert_eq!(eval("let x = 5; x(1)"), Err(AwaError::NotCallable("Num")));
    }

    // ===== Scala 風の文法（ASI・ブロック・Unit・コメント）=====

    #[test]
    fn セミコロン無しでも単文は評価できる() {
        assert_eq!(eval("1 + 2"), Ok(num(3.0)));
    }

    #[test]
    fn 改行が文の区切りになる() {
        let mut env = Env::new();
        assert_eq!(
            run("let x = 5\nx + 1", &mut env),
            Ok(vec![Value::Num(5.0), Value::Num(6.0)])
        );
    }

    #[test]
    fn 演算子の直後の改行は継続扱い() {
        assert_eq!(eval("1 +\n2 +\n3"), Ok(num(6.0)));
    }

    #[test]
    fn 区切り無しで式が連続するとエラー() {
        assert_eq!(eval("1 2"), Err(AwaError::UnexpectedToken("Num(2.0)".to_string())));
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
        assert_eq!(eval("let add = fn(a, b) { a + b }; add(\n1,\n2\n)"), Ok(num(3.0)));
    }
}
