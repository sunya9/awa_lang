// awa コマンド（CLI）。コンパイラ本体は awa-core ライブラリにあり、ここは
// 引数解釈・ファイル I/O・clang 呼び出し・REPL といった「実行環境」だけを持つ。
use std::io::{Write, stdout};
use std::path::PathBuf;

use awa_core::ast::{self, TStmt};
use awa_core::error::report;
use awa_core::eval::{self, eval_program};
use awa_core::infer::Checker;
use awa_core::types::{Type, render_type};
use awa_core::value::Value;
use awa_core::{
    ASYNC_RUNTIME_C, LoadedModule, check_program_modules, compile_modules_to_ir, format_error,
    load_program, run,
};

// CLI のエントリパスを (root, エントリファイル名) に分解する。
// root はエントリの親ディレクトリ（モジュール解決の基点）。`import util` は root/util.awa を指す。
// 例: "examples/app/main.awa" → (root="examples/app", name="main.awa")。
fn entry_root_and_name(path: &str) -> (PathBuf, String) {
    let p = std::path::Path::new(path);
    let root = match p.parent() {
        Some(d) if !d.as_os_str().is_empty() => d.to_path_buf(),
        _ => PathBuf::from("."),
    };
    let name = p
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.to_string());
    (root, name)
}

// 標準ライブラリの「ディスク上書き」パス。`import std/...` の既定はバイナリ焼き込み
// （awa-core の stdlib モジュール）で解決されるので、通常は None で構わない。
// ここで返したパスに実ファイルがあれば焼き込みより優先される＝標準ライブラリ自体を
// 編集しながら試すための上書き。
//   1) 環境変数 AWA_STDLIB（明示的に別の std/ を指す）
//   2) カレントディレクトリに std/ があればそれ（リポジトリルートでの開発用）
fn stdlib_root() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("AWA_STDLIB") {
        return Some(PathBuf::from(p));
    }
    let cwd = std::env::current_dir().ok()?;
    if cwd.join("std").is_dir() {
        Some(cwd)
    } else {
        None
    }
}

// エントリパスからローダを走らせて全モジュールを得る。失敗したら整形済みメッセージを出して終了。
fn load_or_exit(path: &str) -> Vec<LoadedModule> {
    let (root, name) = entry_root_and_name(path);
    load_program(&root, stdlib_root().as_deref(), &name).unwrap_or_else(|msg| {
        eprintln!("{}", msg);
        std::process::exit(1);
    })
}

// ===== ファイル実行 =====
// エントリ .awa を import ごと読み込み、名前解決→型検査→評価して一括実行する。
// 表示するのはエントリモジュールの行だけ（import したモジュールの定義は出力しない）。
fn run_file(path: &str) {
    let modules = load_or_exit(path);
    // エントリ（依存順で最後）の Let/Expr 文数＝表示する行数。
    let entry_rows = modules
        .last()
        .map(|m| {
            m.stmts
                .iter()
                .filter(|s| matches!(s, ast::Stmt::Let(..) | ast::Stmt::Expr(..)))
                .count()
        })
        .unwrap_or(0);
    let typed = match check_program_modules(&modules) {
        Ok(t) => t,
        Err(msg) => {
            eprintln!("{}", msg);
            std::process::exit(1);
        }
    };
    let mut env = eval::builtins_env();
    let values = match eval_program(&typed, &mut env) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("{}", format_error(&e, &modules));
            std::process::exit(1);
        }
    };
    let types = typed.iter().filter_map(|ts| match ts {
        TStmt::Let(_, te) | TStmt::Expr(te) => Some(te.ty.clone()),
        TStmt::Type(_) | TStmt::Impl(_) => None,
    });
    let rows: Vec<(Value, Type)> = values.into_iter().zip(types).collect();
    let start = rows.len().saturating_sub(entry_rows);
    for (v, t) in &rows[start..] {
        // Unit（Nil）の結果行は表示しない（print/write_file 等の副作用文のノイズを抑える）。
        if matches!(v, Value::Unit) {
            continue;
        }
        println!("{} : {}", v, render_type(t));
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

// `awa emit <file>`: LLVM IR を標準出力へ（import を辿って全モジュールを結合）。
fn emit_file(path: &str) {
    let modules = load_or_exit(path);
    match compile_modules_to_ir(&modules) {
        Ok(ir) => print!("{}", ir),
        Err(msg) => {
            eprintln!("{}", msg);
            std::process::exit(1);
        }
    }
}

// `awa build <file>`: IR を生成し clang でネイティブ実行ファイルを作る。
// out_name が Some なら実行ファイル名をそれにする（awa.toml の name 用）。
fn build_file(path: &str, out_name: Option<&str>) {
    let modules = load_or_exit(path);
    let ir = match compile_modules_to_ir(&modules) {
        Ok(ir) => ir,
        Err(msg) => {
            eprintln!("{}", msg);
            std::process::exit(1);
        }
    };
    let base = path.strip_suffix(".awa").unwrap_or(path);
    let exe = out_name.unwrap_or(base);
    let ll_path = format!("{}.ll", base);
    if let Err(e) = std::fs::write(&ll_path, &ir) {
        eprintln!("IR を書けません ({}): {}", ll_path, e);
        std::process::exit(1);
    }
    // 非同期イベントループの C ランタイムも書き出して一緒にリンクする。
    let rt_path = format!("{}.async_rt.c", base);
    if let Err(e) = std::fs::write(&rt_path, ASYNC_RUNTIME_C) {
        eprintln!("ランタイムを書けません ({}): {}", rt_path, e);
        std::process::exit(1);
    }
    // -O2: LLVM の sibling-call 最適化で末尾再帰が定数スタックになる（ループ相当）。
    // 併せて未使用関数の除去（globaldce）など一般的な最適化も効く。
    let status = std::process::Command::new("clang")
        .args(["-O2", &rt_path, &ll_path, "-o", exe])
        .status();
    match status {
        Ok(s) if s.success() => println!("built: {}", exe),
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

// `awa fmt [-w|--check] <file>`: 整形して stdout へ。-w で上書き、--check で差分検査。
fn fmt_file(args: &[String]) {
    let (mode, path) = match args.get(2).map(|s| s.as_str()) {
        Some("-w") | Some("--write") => ("write", args.get(3)),
        Some("--check") => ("check", args.get(3)),
        Some(_) => ("stdout", args.get(2)),
        None => ("stdout", None),
    };
    let Some(path) = path else {
        eprintln!("使い方: awa fmt [-w|--check] <file.awa>");
        std::process::exit(1);
    };
    let src = match std::fs::read_to_string(path) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("読めません ({}): {}", path, e);
            std::process::exit(1);
        }
    };
    let formatted = match awa_core::fmt::format_source(&src) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("{}", awa_core::error::report(&e, &src));
            std::process::exit(1);
        }
    };
    match mode {
        "write" => {
            if let Err(e) = std::fs::write(path, &formatted) {
                eprintln!("書けません ({}): {}", path, e);
                std::process::exit(1);
            }
        }
        "check" => {
            if src != formatted {
                eprintln!("整形されていません: {}", path);
                std::process::exit(1);
            }
        }
        _ => print!("{}", formatted),
    }
}

// プロジェクトマニフェスト awa.toml（最小）。name と entry だけ見る。
struct Manifest {
    name: Option<String>,
    entry: Option<String>,
}

// 最小 TOML パーサ: `key = "value"` 行のみ。コメント(#)・空行・[section] は無視。
fn parse_manifest(src: &str) -> Manifest {
    let mut name = None;
    let mut entry = None;
    for line in src.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with('[') {
            continue;
        }
        if let Some((k, v)) = line.split_once('=') {
            let v = v.trim().trim_matches('"').to_string();
            match k.trim() {
                "name" => name = Some(v),
                "entry" => entry = Some(v),
                _ => {}
            }
        }
    }
    Manifest { name, entry }
}

fn load_manifest() -> Option<Manifest> {
    std::fs::read_to_string("awa.toml")
        .ok()
        .map(|s| parse_manifest(&s))
}

// `awa init [name]`: awa.toml と src/main.awa の雛形を作る。
fn init_project(name: Option<&str>) {
    if std::path::Path::new("awa.toml").exists() {
        eprintln!("awa.toml は既に存在します");
        std::process::exit(1);
    }
    let name = name.map(String::from).unwrap_or_else(|| {
        std::env::current_dir()
            .ok()
            .and_then(|p| p.file_name().map(|s| s.to_string_lossy().into_owned()))
            .unwrap_or_else(|| "app".to_string())
    });
    let manifest = format!(
        "# awa プロジェクト設定\nname = \"{name}\"\nversion = \"0.1.0\"\nentry = \"src/main.awa\"\n"
    );
    if let Err(e) = std::fs::write("awa.toml", manifest) {
        eprintln!("awa.toml を書けません: {e}");
        std::process::exit(1);
    }
    let _ = std::fs::create_dir_all("src");
    if !std::path::Path::new("src/main.awa").exists() {
        let main_src = "import std/io\n\nlet main = fn() {\n  io.println(\"Hello from awa!\")\n}\n";
        if let Err(e) = std::fs::write("src/main.awa", main_src) {
            eprintln!("src/main.awa を書けません: {e}");
            std::process::exit(1);
        }
    }
    println!("created awa.toml and src/main.awa");
    println!("  awa         # インタプリタ実行");
    println!("  awa build   # ネイティブビルド");
}

fn main() {
    // インタプリタは eval_expr が Rust 再帰なので、深い（末尾）再帰でスタックが溢れる。
    // 大きなスタックのスレッドで実行して実用的な深さを吸収する（native は -O2 の TCO で定数スタック）。
    let child = std::thread::Builder::new()
        .stack_size(1024 * 1024 * 1024) // 1 GiB
        .spawn(run_cli)
        .expect("スレッド起動");
    child.join().expect("実行スレッド");
}

fn run_cli() {
    // 引数: awa emit/build [file] はコンパイル、awa fmt は整形、awa init は雛形生成、
    // awa <file> はインタプリタ実行。ファイル省略時は awa.toml の entry、無ければ REPL。
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(|s| s.as_str()) {
        Some("emit") | Some("build") => {
            let is_emit = args[1] == "emit";
            // ファイル引数があればそれ、無ければ awa.toml の entry。
            let (path, out_name) = match args.get(2) {
                Some(p) => (p.clone(), None),
                None => {
                    let m = load_manifest().unwrap_or_else(|| {
                        eprintln!("使い方: awa {} <file.awa>（または awa.toml に entry）", args[1]);
                        std::process::exit(1);
                    });
                    let entry = m.entry.unwrap_or_else(|| {
                        eprintln!("awa.toml に entry がありません");
                        std::process::exit(1);
                    });
                    (entry, m.name)
                }
            };
            if is_emit {
                emit_file(&path);
            } else {
                build_file(&path, out_name.as_deref());
            }
        }
        Some("fmt") => fmt_file(&args),
        Some("init") => init_project(args.get(2).map(|s| s.as_str())),
        Some(path) => run_file(path),
        None => {
            // 引数なし: awa.toml の entry を実行、無ければ REPL。
            if let Some(entry) = load_manifest().and_then(|m| m.entry) {
                run_file(&entry);
                return;
            }
            if let Err(e) = repl() {
                eprintln!("REPL エラー: {}", e);
                std::process::exit(1);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::parse_manifest;

    #[test]
    fn manifest_の最小パース() {
        let src = "# project\nname = \"demo\"\nversion = \"0.1.0\"\nentry = \"src/main.awa\"\n";
        let m = parse_manifest(src);
        assert_eq!(m.name.as_deref(), Some("demo"));
        assert_eq!(m.entry.as_deref(), Some("src/main.awa"));
    }

    #[test]
    fn manifest_は欠落キーを許容() {
        let m = parse_manifest("name = \"x\"\n");
        assert_eq!(m.name.as_deref(), Some("x"));
        assert_eq!(m.entry, None);
    }
}
