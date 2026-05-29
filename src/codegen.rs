use std::collections::{HashMap, HashSet};

use crate::ast::{Pattern, TExpr, TExprKind, TStmt, TypeExpr, TypeExprKind};
use crate::error::AwaError;
use crate::token::Token;
use crate::types::Type;

// ===== LLVM コード生成（バックエンド・マイルストーン2: クロージャ）=====
// 型付き AST → LLVM IR テキスト(.ll)。clang file.ll -o exe でネイティブ化。
//
// 値表現: Int/Long=i64、Float=double、Bool=i1、関数=クロージャ構造体 { ptr fn, ptr env }。
// クロージャ変換: すべての関数は第一引数に env ポインタを取り、本体冒頭でキャプチャ変数を
// env から load する。トップレベル関数はキャプチャなし（env=null・直接呼び出し）。ネスト lambda は
// 外側スコープの変数を malloc した env に詰めて捕獲する（解放しない＝v1 はリーク許容）。
// 未対応（ADT/String/case 等）は AwaError::Unsupported を返す（誤コンパイルしない）。

type Result<T> = std::result::Result<T, AwaError>;
const CLOSURE_TY: &str = "{ ptr, ptr }";
// 全ヒープオブジェクト共通の先頭ヘッダ: { i64 rc, ptr drop_fn }（16 バイト）。
// rc = 参照カウント（負値はリテラル等の「不滅」）、drop_fn = 解放前に子を release する関数。
// 物理レイアウトは rc=offset 0、drop=offset 8。文字列本体やフィールドはこの後ろに続く。
const HDR_SIZE: i64 = 16;

fn unsupported<T>(what: impl Into<String>) -> Result<T> {
    Err(AwaError::Unsupported(what.into()))
}

// awa のグローバル名を LLVM の `@`-シンボル token にする。`#`（メソッドのマングル "型頭#名"）など
// 非英数字を含む名前はクォート識別子 @"..." にする（LLVM は受理する）。"." "_" "$" は素のまま可。
fn sym(name: &str) -> String {
    if name
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'$'))
    {
        format!("@{}", name)
    } else {
        format!("@\"{}\"", name)
    }
}

// awa の型 → LLVM 型。関数型はクロージャ構造体、ADT はヒープブロックへの ptr。
fn llvm_ty(ty: &Type) -> Result<String> {
    match ty {
        Type::Int | Type::Long => Ok("i64".into()),
        Type::Float => Ok("double".into()),
        Type::Bool => Ok("i1".into()),
        Type::Str => Ok("ptr".into()),
        Type::Fun(..) => Ok(CLOSURE_TY.into()),
        // Future（イベントループの id）と Socket（fd）はランタイムのハンドル＝i64。
        Type::Con(name, _) if name == "Future" || name == "Socket" => Ok("i64".into()),
        Type::Con(..) => Ok("ptr".into()),
        // Unit は値を持たないが、I/O 等の戻り値として流れるのでダミーの i8 で表す。
        Type::Unit => Ok("i8".into()),
        other => unsupported(format!("型 {}", other)),
    }
}

// 数値変換などの組込関数（codegen 側）。
fn is_builtin(name: &str) -> bool {
    matches!(
        name,
        "int_to_float"
            | "float_to_int"
            | "int_to_long"
            | "long_to_int"
            | "int_to_string"
            | "float_to_string"
    )
}

// 非同期の組込（イベントループランタイムへ委譲する）。
fn is_async_builtin(name: &str) -> bool {
    matches!(name, "pure" | "delay" | "run" | "Future#flatMap" | "Future#map")
}

// OS 連携の同期 I/O 組込（C ランタイムへ委譲する）。
fn is_io_builtin(name: &str) -> bool {
    matches!(name, "print" | "read_line" | "read_file" | "write_file" | "now")
}

// OS 連携のソケット組込（kqueue イベントループへ委譲する）。
fn is_socket_builtin(name: &str) -> bool {
    matches!(
        name,
        "tcp_listen" | "sock_port" | "accept" | "sock_read" | "sock_write" | "sock_close" | "tcp_connect"
    )
}

// 呼び出し対象の戻り型（callee.ty = Fun(params, ret) の ret）。
fn callee_ret(callee: &TExpr) -> Result<Type> {
    match &callee.ty {
        Type::Fun(_, ret) => Ok((**ret).clone()),
        _ => unsupported("呼び出し対象が関数型でない"),
    }
}

// flatMap/map 用のトランポリン関数を生成して名前を返す。
// シグネチャ: i64 @trampN(ptr %state, i64 %boxed)。%state は保存した継続クロージャ {fn,env}。
// boxed を a 型へアンボックス → 継続 k を呼ぶ。flatMap は k の戻り（Future=i64）をそのまま返し、
// map は k の戻り（b）をボックス化して返す。ランタイムは常にこの均一シグネチャで呼ぶ。
fn gen_trampoline(m: &mut Module, a_ty: &Type, b_ty: &Type, is_map: bool) -> Result<String> {
    let name = m.fresh_fn();
    let a_ll = llvm_ty(a_ty)?;
    let mut g = Gen::new();
    // a へアンボックス。
    let a = g.unbox_word("%boxed", a_ty)?;
    // 継続クロージャ {fn,env} を state から load。
    let kfn = format!("%t{}", g.fresh());
    g.line(&format!("{} = load ptr, ptr %state", kfn));
    let ep = format!("%t{}", g.fresh());
    g.line(&format!("{} = getelementptr i8, ptr %state, i64 8", ep));
    let kenv = format!("%t{}", g.fresh());
    g.line(&format!("{} = load ptr, ptr {}", kenv, ep));
    // k(env, a) を呼ぶ。flatMap なら戻りは i64(Future)、map なら b。
    let ret_ll = if is_map { llvm_ty(b_ty)? } else { "i64".to_string() };
    let res = format!("%t{}", g.fresh());
    g.line(&format!(
        "{} = call {} {}(ptr {}, {} {})",
        res, ret_ll, kfn, kenv, a_ll, a
    ));
    if is_map {
        let boxed = g.box_word(&res, b_ty)?;
        g.line(&format!("ret i64 {}", boxed));
    } else {
        g.line(&format!("ret i64 {}", res));
    }
    m.funcs.push_str(&format!(
        "define i64 @{}(ptr %state, i64 %boxed) {{\n{}}}\n\n",
        name, g.out
    ));
    Ok(name)
}

// Rust 文字列 → LLVM の文字列定数 `[N x i8] c"...\00"` と長さ N（null 込み）。
fn llvm_string_constant(s: &str) -> (String, usize) {
    let mut body = String::new();
    for &b in s.as_bytes() {
        if b == b'"' || b == b'\\' || !(0x20..0x7f).contains(&b) {
            body.push_str(&format!("\\{:02X}", b));
        } else {
            body.push(b as char);
        }
    }
    body.push_str("\\00");
    let n = s.len() + 1;
    (format!("[{} x i8] c\"{}\"", n, body), n)
}

// 構築子の情報（タグ・arity・所属型・フィールド型式・型パラメータ）。
#[derive(Clone)]
struct CtorInfo {
    tag: u32,
    arity: usize,
    type_params: Vec<String>,
    field_texprs: Vec<TypeExpr>,
}

// 型式＋param→具体型 の対応で、ADT フィールドの具体型を求める（単相前提）。
fn resolve_field(texpr: &TypeExpr, param_map: &HashMap<String, Type>) -> Result<Type> {
    match &texpr.kind {
        TypeExprKind::Var(p) => param_map
            .get(p)
            .cloned()
            .ok_or_else(|| AwaError::Unsupported(format!("型パラメータ {}", p))),
        TypeExprKind::Con(name, args) => {
            let resolved: Vec<Type> = args
                .iter()
                .map(|a| resolve_field(a, param_map))
                .collect::<Result<_>>()?;
            Ok(match name.as_str() {
                "Int" => Type::Int,
                "Long" => Type::Long,
                "Float" => Type::Float,
                "String" => Type::Str,
                "Bool" => Type::Bool,
                "Nil" => Type::Unit,
                _ => Type::Con(name.clone(), resolved),
            })
        }
        TypeExprKind::Fun(ps, r) => {
            let ps: Vec<Type> = ps
                .iter()
                .map(|p| resolve_field(p, param_map))
                .collect::<Result<_>>()?;
            Ok(Type::Fun(ps, Box::new(resolve_field(r, param_map)?)))
        }
    }
}

// or パターンを or-free のパターン集合へ展開（入れ子は直積）。compile_case が複製アームに使う。
fn expand_pattern(p: &Pattern) -> Vec<Pattern> {
    match p {
        Pattern::Or(alts) => alts.iter().flat_map(expand_pattern).collect(),
        Pattern::Ctor(name, subs) => {
            let ex: Vec<Vec<Pattern>> = subs.iter().map(expand_pattern).collect();
            cartesian(&ex)
                .into_iter()
                .map(|c| Pattern::Ctor(name.clone(), c))
                .collect()
        }
        other => vec![other.clone()],
    }
}

fn cartesian(lists: &[Vec<Pattern>]) -> Vec<Vec<Pattern>> {
    let mut acc = vec![Vec::new()];
    for list in lists {
        let mut next = Vec::new();
        for prefix in &acc {
            for item in list {
                let mut row = prefix.clone();
                row.push(item.clone());
                next.push(row);
            }
        }
        acc = next;
    }
    acc
}

// 構築子ブロックの LLVM 型: { i64 rc, ptr drop, i32 tag, field0, ... }。
// 先頭 2 要素は RC ヘッダ。tag は index 2、フィールドは index 3+i に並ぶ。
fn block_ty(field_tys: &[Type]) -> Result<String> {
    let mut parts = vec!["i64".to_string(), "ptr".to_string(), "i32".to_string()];
    for t in field_tys {
        parts.push(llvm_ty(t)?);
    }
    Ok(format!("{{ {} }}", parts.join(", ")))
}

fn is_float(ty: &Type) -> bool {
    matches!(ty, Type::Float)
}

// env 構造体の LLVM 型文字列（RC ヘッダ＋キャプチャ型の並び）。
// 先頭 2 要素は RC ヘッダ。キャプチャは index 2+i に並ぶ。
fn env_struct_ty(caps: &[(String, Type)]) -> Result<String> {
    let mut parts = vec!["i64".to_string(), "ptr".to_string()];
    for (_, t) in caps {
        parts.push(llvm_ty(t)?);
    }
    Ok(format!("{{ {} }}", parts.join(", ")))
}

// ===== モジュール全体の状態 =====
struct Module {
    funcs: String,                          // 全関数定義
    fn_ctr: usize,                          // 匿名関数 @fnN の採番
    globals: HashMap<String, GlobalFn>,     // トップレベル関数名 → 定義（本体つき）
    value_globals: HashMap<String, Type>,   // トップレベルの値 let 名 → 型（LLVM グローバル変数）
    instances: HashSet<String>,             // 発行済みの特殊化インスタンス（マングル名）
    ctors: HashMap<String, CtorInfo>,       // 構築子名 → 情報
    strings: String,                        // 文字列リテラルのグローバル定義
    str_ctr: usize,
    drops: HashSet<String>,                 // 生成済み drop_fn 名（フィールド型レイアウトごと）
}

// トップレベル関数の定義（単相化で本体を特殊化するため本体も持つ）。
#[derive(Clone)]
struct GlobalFn {
    param_names: Vec<String>,
    param_tys: Vec<Type>,
    ret: Type,
    body: TExpr,
}

impl GlobalFn {
    fn is_generic(&self) -> bool {
        self.param_tys.iter().any(has_var) || has_var(&self.ret)
    }
}

fn has_var(t: &Type) -> bool {
    match t {
        Type::Var(_) => true,
        Type::Fun(ps, r) => ps.iter().any(has_var) || has_var(r),
        Type::Con(_, args) => args.iter().any(has_var),
        _ => false,
    }
}

// 総称シグネチャ（型変数つき）を具体型に照合して置換を作る。
fn match_types(g: &Type, c: &Type, s: &mut crate::types::Subst) {
    match (g, c) {
        (Type::Var(id), _) => {
            s.insert(*id, c.clone());
        }
        (Type::Fun(gp, gr), Type::Fun(cp, cr)) => {
            for (a, b) in gp.iter().zip(cp.iter()) {
                match_types(a, b, s);
            }
            match_types(gr, cr, s);
        }
        (Type::Con(_, ga), Type::Con(_, ca)) => {
            for (a, b) in ga.iter().zip(ca.iter()) {
                match_types(a, b, s);
            }
        }
        _ => {}
    }
}

// 型を LLVM 記号に使える短い名前へ（特殊化のマングル用）。
fn mangle_ty(t: &Type) -> String {
    match t {
        Type::Int => "Int".into(),
        Type::Long => "Long".into(),
        Type::Float => "Float".into(),
        Type::Str => "String".into(),
        Type::Bool => "Bool".into(),
        Type::Unit => "Nil".into(),
        Type::Var(_) => "_".into(),
        Type::Fun(..) => "fn".into(),
        Type::Con(n, args) => {
            if args.is_empty() {
                n.clone()
            } else {
                format!("{}_{}", n, args.iter().map(mangle_ty).collect::<Vec<_>>().join("_"))
            }
        }
    }
}

fn mangle(name: &str, params: &[Type], ret: &Type) -> String {
    let mut parts: Vec<String> = params.iter().map(mangle_ty).collect();
    parts.push(mangle_ty(ret));
    format!("{}${}", name, parts.join("$"))
}

// 型付き AST の全ノードの型に置換を適用する（特殊化＝総称本体を具体化する）。
fn subst_types(te: &mut TExpr, s: &crate::types::Subst) {
    te.ty = crate::types::apply(s, &te.ty);
    match &mut te.kind {
        TExprKind::Int(_)
        | TExprKind::Long(_)
        | TExprKind::Float(_)
        | TExprKind::Str(_)
        | TExprKind::Bool(_)
        | TExprKind::Var(_) => {}
        TExprKind::Neg(x) => subst_types(x, s),
        TExprKind::BinOp(l, _, r) => {
            subst_types(l, s);
            subst_types(r, s);
        }
        TExprKind::If(c, t, e) => {
            subst_types(c, s);
            subst_types(t, s);
            subst_types(e, s);
        }
        TExprKind::Lambda(_, b) => subst_types(b, s),
        TExprKind::Call(c, args) => {
            subst_types(c, s);
            for a in args {
                subst_types(a, s);
            }
        }
        TExprKind::Block(stmts) => {
            for st in stmts {
                match st {
                    TStmt::Let(_, e) | TStmt::Expr(e) => subst_types(e, s),
                    TStmt::Type(_) | TStmt::Impl(_) => {}
                }
            }
        }
        TExprKind::Case(subj, arms) => {
            subst_types(subj, s);
            for (_, b) in arms {
                subst_types(b, s);
            }
        }
    }
}

// 呼び出し対象のトップレベル関数を具体型で特殊化し、呼ぶべき LLVM 名を返す。
// 単相関数は @name（pass2 で発行済み）。総称関数は @name$型... を遅延発行（メモ）。
fn ensure_instance(m: &mut Module, name: &str, params: &[Type], ret: &Type) -> Result<String> {
    let gf = m.globals.get(name).expect("known top-level function").clone();
    if !gf.is_generic() {
        return Ok(name.to_string());
    }
    let mangled = mangle(name, params, ret);
    if m.instances.contains(&mangled) {
        return Ok(mangled);
    }
    m.instances.insert(mangled.clone());
    // 総称シグネチャ → 具体型の置換を作り、本体に適用して特殊化。
    let mut s = crate::types::Subst::new();
    for (g, c) in gf.param_tys.iter().zip(params.iter()) {
        match_types(g, c, &mut s);
    }
    match_types(&gf.ret, ret, &mut s);
    let mut body = gf.body.clone();
    subst_types(&mut body, &s);
    let typed_params: Vec<(String, Type)> = gf
        .param_names
        .iter()
        .cloned()
        .zip(params.iter().cloned())
        .collect();
    gen_function(m, &mangled, &[], &typed_params, ret, &body)?;
    Ok(mangled)
}

impl Module {
    fn fresh_fn(&mut self) -> String {
        let n = self.fn_ctr;
        self.fn_ctr += 1;
        format!("fn{}", n)
    }
    // 文字列リテラルをグローバル定数として登録し、その ptr（@.strN）を返す。
    // RC ヘッダ込みのレイアウト { i64 rc, ptr drop, [N x i8] bytes }。リテラルは不滅なので
    // rc=-1（awa_release は負値を解放しない）、drop=null。本体は obj+16（str_body で取る）。
    fn intern_string(&mut self, s: &str) -> String {
        let label = format!("@.str{}", self.str_ctr);
        self.str_ctr += 1;
        let (arr, n) = llvm_string_constant(s);
        let arr_ty = format!("[{} x i8]", n);
        self.strings.push_str(&format!(
            "{} = private unnamed_addr constant {{ i64, ptr, {} }} {{ i64 -1, ptr null, {} }}\n",
            label, arr_ty, arr
        ));
        label
    }
}

// ===== 1 関数本体の生成状態 =====
struct Gen {
    out: String,
    reg: usize,
    scope: HashMap<String, (String, Type)>, // 変数名 → (alloca ptr, 型)
}

impl Gen {
    fn new() -> Self {
        Gen {
            out: String::new(),
            reg: 0,
            scope: HashMap::new(),
        }
    }
    fn fresh(&mut self) -> usize {
        let n = self.reg;
        self.reg += 1;
        n
    }
    fn line(&mut self, s: &str) {
        self.out.push_str("  ");
        self.out.push_str(s);
        self.out.push('\n');
    }
    fn label(&mut self, l: &str) {
        self.out.push_str(l);
        self.out.push_str(":\n");
    }
    // ヒープオブジェクトの RC ヘッダを初期化: rc=1（offset 0）、drop_fn=null（offset 8）。
    // drop_fn の中身は M2（回収有効化）で型ごとに埋める。物理オフセットで書くので型不問。
    fn init_header(&mut self, obj: &str) -> Result<()> {
        self.line(&format!("store i64 1, ptr {}", obj));
        let dp = format!("%t{}", self.fresh());
        self.line(&format!("{} = getelementptr i8, ptr {}, i64 8", dp, obj));
        self.line(&format!("store ptr null, ptr {}", dp));
        // 生存数 ++（リーク検出）。
        let lv = format!("%t{}", self.fresh());
        self.line(&format!("{} = load i64, ptr @awa_live", lv));
        let lv1 = format!("%t{}", self.fresh());
        self.line(&format!("{} = add i64 {}, 1", lv1, lv));
        self.line(&format!("store i64 {}, ptr @awa_live", lv1));
        Ok(())
    }

    // 文字列オブジェクトの本体（null 終端バイト列）への ptr = obj + ヘッダ長。
    // C 関数（strcmp/printf/strlen/memcpy）にはこの本体 ptr を渡す。
    fn str_body(&mut self, obj: &str) -> String {
        let r = format!("%t{}", self.fresh());
        self.line(&format!("{} = getelementptr i8, ptr {}, i64 {}", r, obj, HDR_SIZE));
        r
    }

    // 値の所有権を 1 増やす（owning convention の「変数を読む＝新たな所有参照を作る」）。
    // ヒープ型のみ対象。スカラ（Int/Bool 等）は何もしない。
    // 文字列(M2a)・ADT(M2b) は ptr 直接、クロージャ(M2c) は env ptr に対して行う。
    fn retain(&mut self, val: &str, ty: &Type) {
        match ty {
            // Future/Socket は i64 ハンドル（ヒープでない）→ RC 対象外。
            Type::Str | Type::Con(..) if is_heap(ty) => {
                self.line(&format!("call void @awa_retain(ptr {})", val));
            }
            Type::Fun(..) => {
                let env = self.closure_env(val);
                self.line(&format!("call void @awa_retain(ptr {})", env));
            }
            _ => {}
        }
    }

    // 値の所有権を 1 減らす（スコープ脱出・値の破棄）。0 で解放。
    fn release(&mut self, val: &str, ty: &Type) {
        match ty {
            Type::Str | Type::Con(..) if is_heap(ty) => {
                self.line(&format!("call void @awa_release(ptr {})", val));
            }
            Type::Fun(..) => {
                let env = self.closure_env(val);
                self.line(&format!("call void @awa_release(ptr {})", env));
            }
            _ => {}
        }
    }

    // クロージャ値 { ptr fn, ptr env } から env ポインタを取り出す。
    fn closure_env(&mut self, clos: &str) -> String {
        let r = format!("%t{}", self.fresh());
        self.line(&format!("{} = extractvalue {} {}, 1", r, CLOSURE_TY, clos));
        r
    }

    fn bind_local(&mut self, name: &str, operand: &str, ty: &Type) -> Result<()> {
        let llty = llvm_ty(ty)?;
        let slot = format!("%v{}", self.fresh());
        self.line(&format!("{} = alloca {}", slot, llty));
        self.line(&format!("store {} {}, ptr {}", llty, operand, slot));
        self.scope.insert(name.to_string(), (slot, ty.clone()));
        Ok(())
    }

    // 式 → LLVM オペランド。
    fn expr(&mut self, e: &TExpr, m: &mut Module) -> Result<String> {
        match &e.kind {
            TExprKind::Int(n) | TExprKind::Long(n) => Ok(n.to_string()),
            TExprKind::Float(n) => Ok(format!("0x{:016X}", n.to_bits())),
            TExprKind::Bool(b) => Ok(if *b { "1".into() } else { "0".into() }),
            // 文字列リテラル → グローバル定数への ptr。
            TExprKind::Str(s) => Ok(m.intern_string(s)),
            TExprKind::Var(name) => self.var(name, &e.ty, m),
            TExprKind::Neg(x) => {
                let v = self.expr(x, m)?;
                let r = format!("%t{}", self.fresh());
                if is_float(&x.ty) {
                    self.line(&format!("{} = fneg double {}", r, v));
                } else {
                    self.line(&format!("{} = sub i64 0, {}", r, v));
                }
                Ok(r)
            }
            TExprKind::BinOp(l, op, r) => self.binop(l, op, r, m),
            TExprKind::If(c, t, e2) => self.if_expr(c, t, e2, m),
            TExprKind::Call(callee, args) => self.call(callee, args, m),
            TExprKind::Block(stmts) => self.block(stmts, m),
            TExprKind::Lambda(params, body) => self.lambda(params, body, &e.ty, m),
            TExprKind::Case(subject, arms) => self.compile_case(subject, arms, m),
        }
    }

    // case を逐次マッチへコンパイル。各腕でパターンを検査し、不一致なら次の腕へ、
    // 一致したらフィールドを束縛して本体を評価する。網羅性は静的検査済み（最後は unreachable）。
    fn compile_case(
        &mut self,
        subject: &TExpr,
        arms: &[(Pattern, TExpr)],
        m: &mut Module,
    ) -> Result<String> {
        let sv = self.expr(subject, m)?;
        let sty = subject.ty.clone();
        let res_ty = llvm_ty(&arms[0].1.ty)?;
        let res = format!("%v{}", self.fresh());
        self.line(&format!("{} = alloca {}", res, res_ty));
        let end = format!("end{}", self.fresh());
        // or パターンを展開（A | B -> body を A -> body; B -> body に複製）。
        let mut expanded: Vec<(Pattern, &TExpr)> = Vec::new();
        for (pat, body) in arms {
            for p in expand_pattern(pat) {
                expanded.push((p, body));
            }
        }
        for (pat, body) in &expanded {
            let next = format!("arm{}", self.fresh());
            let saved = self.scope.clone();
            self.match_pattern_ir(pat, &sv, &sty, &next, m)?;
            let bv = self.expr(body, m)?;
            self.line(&format!("store {} {}, ptr {}", res_ty, bv, res));
            // この腕が成立 → パターン束縛と subject を release（結果値は別の +1）。
            self.release_scope_since(&saved)?;
            self.release(&sv, &sty);
            self.line(&format!("br label %{}", end));
            self.scope = saved;
            self.label(&next);
        }
        // どの腕にも当たらない経路（網羅性が保証するので到達しない）。
        self.line("unreachable");
        self.label(&end);
        let r = format!("%t{}", self.fresh());
        self.line(&format!("{} = load {}, ptr {}", r, res_ty, res));
        Ok(r)
    }

    // パターンを値に照合する IR を出す。不一致なら fail へ br、一致なら束縛して継続。
    fn match_pattern_ir(
        &mut self,
        pat: &Pattern,
        value: &str,
        value_ty: &Type,
        fail: &str,
        m: &mut Module,
    ) -> Result<()> {
        match pat {
            Pattern::Wildcard => Ok(()),
            // 束縛は subject 内部への新たな所有参照 → retain（腕脱出で release される）。
            Pattern::Var(n) => {
                self.retain(value, value_ty);
                self.bind_local(n, value, value_ty)
            }
            Pattern::Bool(b) => {
                self.eq_or_fail("i1", value, if *b { "1" } else { "0" }, fail);
                Ok(())
            }
            Pattern::Int(n) => {
                let v = n.to_string();
                self.eq_or_fail("i64", value, &v, fail);
                Ok(())
            }
            // 文字列パターンは strcmp で中身比較（本体 ptr 同士）。
            Pattern::Str(s) => {
                let lit = m.intern_string(s);
                let vb = self.str_body(value);
                let lb = self.str_body(&lit);
                let c = format!("%t{}", self.fresh());
                self.line(&format!("{} = call i32 @strcmp(ptr {}, ptr {})", c, vb, lb));
                let eq = format!("%t{}", self.fresh());
                self.line(&format!("{} = icmp eq i32 {}, 0", eq, c));
                let cont = format!("cont{}", self.fresh());
                self.line(&format!("br i1 {}, label %{}, label %{}", eq, cont, fail));
                self.label(&cont);
                Ok(())
            }
            // or は compile_case で展開済み（ここには来ない）。
            Pattern::Or(_) => unsupported("or パターン（展開漏れ）"),
            Pattern::Ctor(name, subs) => {
                let ci = m
                    .ctors
                    .get(name)
                    .cloned()
                    .ok_or_else(|| AwaError::Unsupported(format!("未知の構築子 {}", name)))?;
                // tag は RC ヘッダの後（block_ty の index 2）。
                let tagp = format!("%t{}", self.fresh());
                self.line(&format!("{} = getelementptr i8, ptr {}, i64 {}", tagp, value, HDR_SIZE));
                let tag = format!("%t{}", self.fresh());
                self.line(&format!("{} = load i32, ptr {}", tag, tagp));
                self.eq_or_fail("i32", &tag, &ci.tag.to_string(), fail);
                if subs.is_empty() {
                    return Ok(());
                }
                // フィールド型を解決（型パラメータ → scrutinee の具体型引数）。
                let Type::Con(_, typeargs) = value_ty else {
                    return unsupported("構築子パターンの主語が ADT でない");
                };
                let param_map: HashMap<String, Type> = ci
                    .type_params
                    .iter()
                    .cloned()
                    .zip(typeargs.iter().cloned())
                    .collect();
                let field_tys: Vec<Type> = ci
                    .field_texprs
                    .iter()
                    .map(|t| resolve_field(t, &param_map))
                    .collect::<Result<_>>()?;
                let bty = block_ty(&field_tys)?;
                for (i, (sub, fty)) in subs.iter().zip(field_tys.iter()).enumerate() {
                    let fp = format!("%t{}", self.fresh());
                    self.line(&format!(
                        "{} = getelementptr {}, ptr {}, i32 0, i32 {}",
                        fp,
                        bty,
                        value,
                        i + 3
                    ));
                    let fv = format!("%t{}", self.fresh());
                    self.line(&format!("{} = load {}, ptr {}", fv, llvm_ty(fty)?, fp));
                    self.match_pattern_ir(sub, &fv, fty, fail, m)?;
                }
                Ok(())
            }
        }
    }

    // value == lit なら継続、違えば fail へ。
    fn eq_or_fail(&mut self, llty: &str, value: &str, lit: &str, fail: &str) {
        let c = format!("%t{}", self.fresh());
        self.line(&format!("{} = icmp eq {} {}, {}", c, llty, value, lit));
        let cont = format!("cont{}", self.fresh());
        self.line(&format!("br i1 {}, label %{}, label %{}", c, cont, fail));
        self.label(&cont);
    }

    // 変数参照: ローカル/キャプチャ → load、ヌル構築子 → 構築、トップレベル関数 → クロージャ値。
    fn var(&mut self, name: &str, ty: &Type, m: &mut Module) -> Result<String> {
        if let Some((slot, vty)) = self.scope.get(name).cloned() {
            let llty = llvm_ty(&vty)?;
            let r = format!("%t{}", self.fresh());
            self.line(&format!("{} = load {}, ptr {}", r, llty, slot));
            // 読み出した値は新たな所有参照（スロットは所有を保持し続ける）。
            self.retain(&r, &vty);
            Ok(r)
        } else if let Some(vty) = m.value_globals.get(name).cloned() {
            // トップレベル値 → グローバル変数 @name から load。
            let llty = llvm_ty(&vty)?;
            let r = format!("%t{}", self.fresh());
            self.line(&format!("{} = load {}, ptr @{}", r, llty, name));
            self.retain(&r, &vty);
            Ok(r)
        } else if let Some(ci) = m.ctors.get(name).cloned() {
            if ci.arity == 0 {
                self.construct(&ci, &[], m) // ヌル構築子
            } else {
                unsupported(format!("構築子を値として使う: {}", name))
            }
        } else if m.globals.contains_key(name) {
            // トップレベル関数を値として使う → 具体型で特殊化し、クロージャ {@mangled, null}。
            let Type::Fun(params, ret) = ty else {
                return unsupported("関数値の型が解決できない");
            };
            let mangled = ensure_instance(m, name, params, ret)?;
            let c0 = format!("%t{}", self.fresh());
            self.line(&format!(
                "{} = insertvalue {} undef, ptr {}, 0",
                c0, CLOSURE_TY, sym(&mangled)
            ));
            let c1 = format!("%t{}", self.fresh());
            self.line(&format!(
                "{} = insertvalue {} {}, ptr null, 1",
                c1, CLOSURE_TY, c0
            ));
            Ok(c1)
        } else {
            unsupported(format!("捕獲できない変数: {}（トップレベル値の参照は未対応）", name))
        }
    }

    fn binop(&mut self, l: &TExpr, op: &Token, r: &TExpr, m: &mut Module) -> Result<String> {
        let lv = self.expr(l, m)?;
        let rv = self.expr(r, m)?;
        // 文字列の連結と等価はランタイム関数（ptr 比較ではなく中身で比べる）。
        // 演算子は両オペランドを consume する（結果を作った後に release）。
        if matches!(l.ty, Type::Str) {
            let res = self.str_binop(op, &lv, &rv)?;
            self.release(&lv, &Type::Str);
            self.release(&rv, &Type::Str);
            return Ok(res);
        }
        let llty = llvm_ty(&l.ty)?;
        let float = is_float(&l.ty);
        let reg = format!("%t{}", self.fresh());
        // 整数除算は 0 除算を実行時チェック（0 ならメッセージを出して exit）。
        if matches!(op, Token::Slash) && !float {
            let c = format!("%t{}", self.fresh());
            self.line(&format!("{} = icmp eq i64 {}, 0", c, rv));
            let id = self.fresh();
            let (dz, ok) = (format!("divzero{id}"), format!("divok{id}"));
            self.line(&format!("br i1 {}, label %{}, label %{}", c, dz, ok));
            self.label(&dz);
            self.line("call i32 (ptr, ...) @printf(ptr @.dz)");
            self.line("call void @exit(i32 1)");
            self.line("unreachable");
            self.label(&ok);
            self.line(&format!("{} = sdiv i64 {}, {}", reg, lv, rv));
            return Ok(reg);
        }
        let arith = match op {
            Token::Plus => Some(if float { "fadd" } else { "add" }),
            Token::Minus => Some(if float { "fsub" } else { "sub" }),
            Token::Star => Some(if float { "fmul" } else { "mul" }),
            Token::Slash => Some(if float { "fdiv" } else { "sdiv" }),
            _ => None,
        };
        if let Some(instr) = arith {
            self.line(&format!("{} = {} {} {}, {}", reg, instr, llty, lv, rv));
            return Ok(reg);
        }
        let pred = match (op, float) {
            (Token::Lt, false) => "icmp slt",
            (Token::Gt, false) => "icmp sgt",
            (Token::Le, false) => "icmp sle",
            (Token::Ge, false) => "icmp sge",
            (Token::EqEq, false) => "icmp eq",
            (Token::NotEq, false) => "icmp ne",
            (Token::Lt, true) => "fcmp olt",
            (Token::Gt, true) => "fcmp ogt",
            (Token::Le, true) => "fcmp ole",
            (Token::Ge, true) => "fcmp oge",
            (Token::EqEq, true) => "fcmp oeq",
            (Token::NotEq, true) => "fcmp one",
            (Token::Concat, _) => return unsupported("文字列連結 <>"),
            (other, _) => unreachable!("二項演算子でないトークン: {:?}", other),
        };
        self.line(&format!("{} = {} {} {}, {}", reg, pred, llty, lv, rv));
        Ok(reg)
    }

    // 文字列の連結 <> と等価 ==/!=（中身比較）。
    fn str_binop(&mut self, op: &Token, lv: &str, rv: &str) -> Result<String> {
        let r = format!("%t{}", self.fresh());
        match op {
            Token::Concat => {
                self.line(&format!("{} = call ptr @awa_concat(ptr {}, ptr {})", r, lv, rv));
                Ok(r)
            }
            Token::EqEq | Token::NotEq => {
                let lb = self.str_body(lv);
                let rb = self.str_body(rv);
                let c = format!("%t{}", self.fresh());
                self.line(&format!("{} = call i32 @strcmp(ptr {}, ptr {})", c, lb, rb));
                let pred = if matches!(op, Token::EqEq) { "eq" } else { "ne" };
                self.line(&format!("{} = icmp {} i32 {}, 0", r, pred, c));
                Ok(r)
            }
            _ => unsupported("文字列にこの演算子は使えません"),
        }
    }

    // 組込変換関数（すべて単項）。
    fn builtin_call(&mut self, name: &str, arg: &str, arg_ty: &Type) -> Result<String> {
        let r = format!("%t{}", self.fresh());
        match name {
            "int_to_float" => {
                self.line(&format!("{} = sitofp i64 {} to double", r, arg));
                Ok(r)
            }
            "float_to_int" => {
                self.line(&format!("{} = fptosi double {} to i64", r, arg));
                Ok(r)
            }
            // i64 ↔ i64（nominal 区別のみ）。実体は同じ。
            "int_to_long" | "long_to_int" => Ok(arg.to_string()),
            "int_to_string" | "float_to_string" => {
                let (fmt, llty) = if matches!(arg_ty, Type::Float) {
                    ("@.ff", "double")
                } else {
                    ("@.fi", "i64")
                };
                // ヘッダ込みで確保し、本体（obj+16）に最大 32 バイト書き込む。
                let buf = format!("%t{}", self.fresh());
                self.line(&format!("{} = call ptr @malloc(i64 {})", buf, HDR_SIZE + 32));
                self.init_header(&buf)?;
                let body = self.str_body(&buf);
                let n = format!("%t{}", self.fresh());
                self.line(&format!(
                    "{} = call i32 (ptr, i64, ptr, ...) @snprintf(ptr {}, i64 32, ptr {}, {} {})",
                    n, body, fmt, llty, arg
                ));
                Ok(buf)
            }
            _ => unsupported(format!("組込関数 {}", name)),
        }
    }

    fn if_expr(&mut self, c: &TExpr, t: &TExpr, e: &TExpr, m: &mut Module) -> Result<String> {
        let res_ty = llvm_ty(&t.ty)?;
        let slot = format!("%v{}", self.fresh());
        self.line(&format!("{} = alloca {}", slot, res_ty));
        let cond = self.expr(c, m)?;
        let id = self.fresh();
        let (tl, el, ml) = (format!("then{id}"), format!("else{id}"), format!("merge{id}"));
        self.line(&format!("br i1 {}, label %{}, label %{}", cond, tl, el));
        self.label(&tl);
        let tv = self.expr(t, m)?;
        self.line(&format!("store {} {}, ptr {}", res_ty, tv, slot));
        self.line(&format!("br label %{}", ml));
        self.label(&el);
        let ev = self.expr(e, m)?;
        self.line(&format!("store {} {}, ptr {}", res_ty, ev, slot));
        self.line(&format!("br label %{}", ml));
        self.label(&ml);
        let r = format!("%t{}", self.fresh());
        self.line(&format!("{} = load {}, ptr {}", r, res_ty, slot));
        Ok(r)
    }

    // 構築子ブロックを malloc して tag と各フィールドを格納し、その ptr を返す。
    fn construct(&mut self, ci: &CtorInfo, fields: &[(String, Type)], m: &mut Module) -> Result<String> {
        let field_tys: Vec<Type> = fields.iter().map(|(_, t)| t.clone()).collect();
        let bty = block_ty(&field_tys)?;
        let szp = format!("%t{}", self.fresh());
        self.line(&format!("{} = getelementptr {}, ptr null, i32 1", szp, bty));
        let sz = format!("%t{}", self.fresh());
        self.line(&format!("{} = ptrtoint ptr {} to i64", sz, szp));
        let p = format!("%t{}", self.fresh());
        self.line(&format!("{} = call ptr @malloc(i64 {})", p, sz));
        // RC ヘッダを初期化（rc=1、drop=null）。
        self.init_header(&p)?;
        // ヒープ型フィールドがあれば drop_fn をヘッダ(offset 8)に設定。
        if let Some(dropname) = ensure_drop_fn(m, &field_tys, &bty, 3, "b")? {
            let dp = format!("%t{}", self.fresh());
            self.line(&format!("{} = getelementptr i8, ptr {}, i64 8", dp, p));
            self.line(&format!("store ptr @{}, ptr {}", dropname, dp));
        }
        let tp = format!("%t{}", self.fresh());
        self.line(&format!(
            "{} = getelementptr {}, ptr {}, i32 0, i32 2",
            tp, bty, p
        ));
        self.line(&format!("store i32 {}, ptr {}", ci.tag, tp));
        for (i, (op, ty)) in fields.iter().enumerate() {
            let fp = format!("%t{}", self.fresh());
            self.line(&format!(
                "{} = getelementptr {}, ptr {}, i32 0, i32 {}",
                fp,
                bty,
                p,
                i + 3
            ));
            self.line(&format!("store {} {}, ptr {}", llvm_ty(ty)?, op, fp));
        }
        Ok(p)
    }

    // 値を均一な i64 ワードへボックス化（Future のペイロードはランタイムで型消去されるため）。
    // LLVM 表現で分岐: i64（Int/Long/Socket/Future）はそのまま、i1/double/ptr は変換。
    fn box_word(&mut self, operand: &str, ty: &Type) -> Result<String> {
        let r = format!("%t{}", self.fresh());
        match llvm_ty(ty)?.as_str() {
            "i64" => Ok(operand.to_string()),
            "i1" => {
                self.line(&format!("{} = zext i1 {} to i64", r, operand));
                Ok(r)
            }
            "i8" => {
                self.line(&format!("{} = zext i8 {} to i64", r, operand));
                Ok(r)
            }
            "double" => {
                self.line(&format!("{} = bitcast double {} to i64", r, operand));
                Ok(r)
            }
            "ptr" => {
                self.line(&format!("{} = ptrtoint ptr {} to i64", r, operand));
                Ok(r)
            }
            other => unsupported(format!("Future に積めない型表現: {}", other)),
        }
    }

    // i64 ワードを本来の型へアンボックス。
    fn unbox_word(&mut self, operand: &str, ty: &Type) -> Result<String> {
        let r = format!("%t{}", self.fresh());
        match llvm_ty(ty)?.as_str() {
            "i64" => Ok(operand.to_string()),
            "i1" => {
                self.line(&format!("{} = trunc i64 {} to i1", r, operand));
                Ok(r)
            }
            "i8" => {
                self.line(&format!("{} = trunc i64 {} to i8", r, operand));
                Ok(r)
            }
            "double" => {
                self.line(&format!("{} = bitcast i64 {} to double", r, operand));
                Ok(r)
            }
            "ptr" => {
                self.line(&format!("{} = inttoptr i64 {} to ptr", r, operand));
                Ok(r)
            }
            other => unsupported(format!("Future から取り出せない型表現: {}", other)),
        }
    }

    // Future(a) の型引数 a を取り出す。
    fn future_arg(ty: &Type) -> Result<Type> {
        match ty {
            Type::Con(n, args) if n == "Future" && args.len() == 1 => Ok(args[0].clone()),
            _ => unsupported("Future 型でない"),
        }
    }

    // 非同期組込（pure/delay/run/Future#flatMap/Future#map）の codegen。
    fn async_call(&mut self, name: &str, callee: &TExpr, args: &[TExpr], m: &mut Module) -> Result<String> {
        match name {
            "pure" => {
                let v = self.expr(&args[0], m)?;
                let boxed = self.box_word(&v, &args[0].ty)?;
                let r = format!("%t{}", self.fresh());
                self.line(&format!("{} = call i64 @awa_future_pure(i64 {})", r, boxed));
                Ok(r)
            }
            "delay" => {
                let ms = self.expr(&args[0], m)?;
                let v = self.expr(&args[1], m)?;
                let boxed = self.box_word(&v, &args[1].ty)?;
                let r = format!("%t{}", self.fresh());
                self.line(&format!(
                    "{} = call i64 @awa_future_delay(i64 {}, i64 {})",
                    r, ms, boxed
                ));
                Ok(r)
            }
            "run" => {
                let f = self.expr(&args[0], m)?;
                let boxed = format!("%t{}", self.fresh());
                self.line(&format!("{} = call i64 @awa_future_run(i64 {})", boxed, f));
                // 戻り値の型は callee の戻り型（Future(a) の a）。
                let Type::Fun(_, ret) = &callee.ty else {
                    return unsupported("run の型が解決できない");
                };
                self.unbox_word(&boxed, ret)
            }
            "Future#flatMap" | "Future#map" => {
                let is_map = name == "Future#map";
                let recv = self.expr(&args[0], m)?; // i64 future
                let a_ty = Self::future_arg(&args[0].ty)?;
                // 結果は Future(b)。
                let b_ty = Self::future_arg(&callee_ret(callee)?)?;
                let k = self.expr(&args[1], m)?; // クロージャ値 {ptr,ptr}
                // k をヒープに保存して state ポインタにする。
                let state = self.store_closure(&k)?;
                let tramp = gen_trampoline(m, &a_ty, &b_ty, is_map)?;
                let rt = if is_map { "awa_future_map" } else { "awa_future_flatmap" };
                let r = format!("%t{}", self.fresh());
                self.line(&format!(
                    "{} = call i64 @{}(i64 {}, ptr @{}, ptr {})",
                    r, rt, recv, tramp, state
                ));
                Ok(r)
            }
            _ => unsupported(format!("非同期組込 {}", name)),
        }
    }

    // クロージャ値 {ptr fn, ptr env} を 16 バイトのヒープに保存し、その ptr を返す
    //（ランタイムが継続の state として保持する）。簡単のため解放しない（async ランタイムの領域）。
    fn store_closure(&mut self, clos: &str) -> Result<String> {
        let p = format!("%t{}", self.fresh());
        self.line(&format!("{} = call ptr @malloc(i64 16)", p));
        let fnp = format!("%t{}", self.fresh());
        self.line(&format!("{} = extractvalue {} {}, 0", fnp, CLOSURE_TY, clos));
        let envp = format!("%t{}", self.fresh());
        self.line(&format!("{} = extractvalue {} {}, 1", envp, CLOSURE_TY, clos));
        self.line(&format!("store ptr {}, ptr {}", fnp, p));
        let ep = format!("%t{}", self.fresh());
        self.line(&format!("{} = getelementptr i8, ptr {}, i64 8", ep, p));
        self.line(&format!("store ptr {}, ptr {}", envp, ep));
        Ok(p)
    }

    // OS 連携の同期 I/O 組込（C ランタイム呼び出し）。String 引数は consume（呼び出し後に release）。
    fn io_call(&mut self, name: &str, args: &[TExpr], m: &mut Module) -> Result<String> {
        match name {
            "print" => {
                let v = self.expr(&args[0], m)?;
                self.line(&format!("call void @awa_print(ptr {})", v));
                self.release(&v, &args[0].ty);
                Ok("0".to_string()) // Unit
            }
            "read_line" => {
                let r = format!("%t{}", self.fresh());
                self.line(&format!("{} = call ptr @awa_read_line()", r));
                Ok(r)
            }
            "read_file" => {
                let v = self.expr(&args[0], m)?;
                let r = format!("%t{}", self.fresh());
                self.line(&format!("{} = call ptr @awa_read_file(ptr {})", r, v));
                self.release(&v, &args[0].ty);
                Ok(r)
            }
            "write_file" => {
                let p = self.expr(&args[0], m)?;
                let c = self.expr(&args[1], m)?;
                self.line(&format!("call void @awa_write_file(ptr {}, ptr {})", p, c));
                self.release(&p, &args[0].ty);
                self.release(&c, &args[1].ty);
                Ok("0".to_string()) // Unit
            }
            "now" => {
                let r = format!("%t{}", self.fresh());
                self.line(&format!("{} = call i64 @awa_now()", r));
                Ok(r)
            }
            _ => unsupported(format!("I/O 組込 {}", name)),
        }
    }

    // OS 連携のソケット組込（C ランタイム呼び出し）。Socket/Future は i64 ハンドル。
    fn sock_call(&mut self, name: &str, args: &[TExpr], m: &mut Module) -> Result<String> {
        let r = format!("%t{}", self.fresh());
        match name {
            "tcp_listen" => {
                let p = self.expr(&args[0], m)?;
                self.line(&format!("{} = call i64 @awa_tcp_listen(i64 {})", r, p));
                Ok(r)
            }
            "sock_port" => {
                let s = self.expr(&args[0], m)?;
                self.line(&format!("{} = call i64 @awa_sock_port(i64 {})", r, s));
                Ok(r)
            }
            "accept" => {
                let s = self.expr(&args[0], m)?;
                self.line(&format!("{} = call i64 @awa_accept(i64 {})", r, s));
                Ok(r)
            }
            "sock_read" => {
                let s = self.expr(&args[0], m)?;
                self.line(&format!("{} = call i64 @awa_sock_read(i64 {})", r, s));
                Ok(r)
            }
            "sock_write" => {
                let s = self.expr(&args[0], m)?;
                let data = self.expr(&args[1], m)?;
                self.line(&format!("{} = call i64 @awa_sock_write(i64 {}, ptr {})", r, s, data));
                self.release(&data, &args[1].ty); // String 引数を consume
                Ok(r)
            }
            "sock_close" => {
                let s = self.expr(&args[0], m)?;
                self.line(&format!("call void @awa_sock_close(i64 {})", s));
                Ok("0".to_string()) // Unit
            }
            "tcp_connect" => {
                let host = self.expr(&args[0], m)?;
                let port = self.expr(&args[1], m)?;
                self.line(&format!("{} = call i64 @awa_tcp_connect(ptr {}, i64 {})", r, host, port));
                self.release(&host, &args[0].ty); // String 引数を consume
                Ok(r)
            }
            _ => unsupported(format!("ソケット組込 {}", name)),
        }
    }

    fn call(&mut self, callee: &TExpr, args: &[TExpr], m: &mut Module) -> Result<String> {
        // 引数の型と値。
        let mut arg_parts = Vec::with_capacity(args.len());
        // 非同期組込（pure/delay/run/Future#flatMap/Future#map）。
        if let TExprKind::Var(name) = &callee.kind {
            if is_async_builtin(name) {
                return self.async_call(name, callee, args, m);
            }
            if is_io_builtin(name) {
                return self.io_call(name, args, m);
            }
            if is_socket_builtin(name) {
                return self.sock_call(name, args, m);
            }
        }
        // callee が組込変換関数なら専用 IR（すべて単項）。
        if let TExprKind::Var(name) = &callee.kind {
            if is_builtin(name) {
                let v = self.expr(&args[0], m)?;
                return self.builtin_call(name, &v, &args[0].ty);
            }
        }
        // callee が構築子なら、引数を評価して Data ブロックを構築。
        if let TExprKind::Var(name) = &callee.kind {
            if let Some(ci) = m.ctors.get(name).cloned() {
                let mut fields = Vec::with_capacity(args.len());
                for a in args {
                    let v = self.expr(a, m)?;
                    fields.push((v, a.ty.clone()));
                }
                return self.construct(&ci, &fields, m);
            }
        }
        // callee がトップレベル関数名なら直接呼び出し（env=null）。具体型で特殊化（単相化）。
        if let TExprKind::Var(name) = &callee.kind {
            if m.globals.contains_key(name) {
                let Type::Fun(cparams, cret) = &callee.ty else {
                    return unsupported("関数呼び出しの型が解決できない");
                };
                let cparams = cparams.clone();
                let cret = (**cret).clone();
                let mangled = ensure_instance(m, name, &cparams, &cret)?;
                let ret_ll = llvm_ty(&cret)?;
                for (a, pty) in args.iter().zip(cparams.iter()) {
                    let v = self.expr(a, m)?;
                    arg_parts.push(format!("{} {}", llvm_ty(pty)?, v));
                }
                let r = format!("%t{}", self.fresh());
                let mut call_args = vec!["ptr null".to_string()];
                call_args.extend(arg_parts);
                self.line(&format!(
                    "{} = call {} {}({})",
                    r,
                    ret_ll,
                    sym(&mangled),
                    call_args.join(", ")
                ));
                return Ok(r);
            }
        }
        // 間接呼び出し: callee をクロージャ値に評価し fn/env を取り出して呼ぶ。
        let Type::Fun(params, ret) = &callee.ty else {
            return unsupported("関数でない値の呼び出し");
        };
        let ret_ll = llvm_ty(ret)?;
        let param_lls: Vec<String> = params.iter().map(llvm_ty).collect::<Result<_>>()?;
        let clos = self.expr(callee, m)?;
        let fnp = format!("%t{}", self.fresh());
        self.line(&format!("{} = extractvalue {} {}, 0", fnp, CLOSURE_TY, clos));
        let envp = format!("%t{}", self.fresh());
        self.line(&format!("{} = extractvalue {} {}, 1", envp, CLOSURE_TY, clos));
        for (a, pll) in args.iter().zip(param_lls.iter()) {
            let v = self.expr(a, m)?;
            arg_parts.push(format!("{} {}", pll, v));
        }
        let r = format!("%t{}", self.fresh());
        let mut call_args = vec![format!("ptr {}", envp)];
        call_args.extend(arg_parts);
        self.line(&format!(
            "{} = call {} {}({})",
            r,
            ret_ll,
            fnp,
            call_args.join(", ")
        ));
        // クロージャ値は consume（呼び出し後に release）。関数は env を借用しただけ。
        self.release(&clos, &callee.ty);
        Ok(r)
    }

    fn block(&mut self, stmts: &[TStmt], m: &mut Module) -> Result<String> {
        let saved = self.scope.clone();
        // 直前の Expr 文の値は、新しい文が来た時点で破棄される → release する。
        let mut last: Option<(String, Type)> = None;
        for s in stmts {
            if let Some((v, ty)) = last.take() {
                self.release(&v, &ty); // 捨てられる中間値
            }
            match s {
                TStmt::Let(name, rhs) => {
                    let v = self.expr(rhs, m)?;
                    self.bind_local(name, &v, &rhs.ty)?; // スロットが所有（retain しない）
                }
                TStmt::Expr(e) => last = Some((self.expr(e, m)?, e.ty.clone())),
                TStmt::Type(_) | TStmt::Impl(_) => return unsupported("ブロック内の type/impl"),
            }
        }
        // このブロックで束縛したローカルを release（結果値は別の +1 所有なので保持される）。
        self.release_scope_since(&saved)?;
        self.scope = saved;
        match last {
            Some((v, _)) => Ok(v),
            None => unsupported("値を持たないブロック"),
        }
    }

    // saved 以降にこのスコープへ追加された束縛を release する（スコープ脱出処理）。
    fn release_scope_since(&mut self, saved: &HashMap<String, (String, Type)>) -> Result<()> {
        let mut locals: Vec<(String, Type)> = Vec::new();
        for (name, (slot, ty)) in &self.scope {
            if !saved.contains_key(name) {
                locals.push((slot.clone(), ty.clone()));
            }
        }
        // 決定的な順序で（名前順でなくスロット順は問わないが安定させる）。
        locals.sort_by(|a, b| a.0.cmp(&b.0));
        for (slot, ty) in locals {
            if is_heap(&ty) {
                let llty = llvm_ty(&ty)?;
                let v = format!("%t{}", self.fresh());
                self.line(&format!("{} = load {}, ptr {}", v, llty, slot));
                self.release(&v, &ty);
            }
        }
        Ok(())
    }

    // ネスト lambda: キャプチャを env(malloc)に詰めてクロージャ値 {@fnN, env} を作る。
    fn lambda(
        &mut self,
        params: &[String],
        body: &TExpr,
        fn_ty: &Type,
        m: &mut Module,
    ) -> Result<String> {
        let Type::Fun(param_tys, ret) = fn_ty else {
            return unsupported("関数の型が解決できない");
        };
        // キャプチャ＝本体の自由変数のうち、トップレベル関数でないもの。型は現スコープから。
        let fv = free_vars(params, body);
        let mut caps: Vec<(String, Type)> = Vec::new();
        for name in &fv {
            // トップレベル関数・値・構築子・組込はグローバル名で参照する（捕獲しない）。
            if m.globals.contains_key(name)
                || m.value_globals.contains_key(name)
                || m.ctors.contains_key(name)
                || is_builtin(name)
                || is_async_builtin(name)
                || is_io_builtin(name)
                || is_socket_builtin(name)
            {
                continue;
            }
            match self.scope.get(name) {
                Some((_, ty)) => caps.push((name.clone(), ty.clone())),
                None => return unsupported(format!("捕獲できない変数: {}", name)),
            }
        }
        let fn_name = m.fresh_fn();
        let typed_params: Vec<(String, Type)> = params
            .iter()
            .cloned()
            .zip(param_tys.iter().cloned())
            .collect();
        gen_function(m, &fn_name, &caps, &typed_params, ret, body)?;

        // env を malloc してキャプチャを格納。
        let env_operand = if caps.is_empty() {
            "null".to_string()
        } else {
            let env_ty = env_struct_ty(&caps)?;
            // sizeof(env) を getelementptr で求める。
            let szp = format!("%t{}", self.fresh());
            self.line(&format!(
                "{} = getelementptr {}, ptr null, i32 1",
                szp, env_ty
            ));
            let sz = format!("%t{}", self.fresh());
            self.line(&format!("{} = ptrtoint ptr {} to i64", sz, szp));
            let env = format!("%t{}", self.fresh());
            self.line(&format!("{} = call ptr @malloc(i64 {})", env, sz));
            self.init_header(&env)?;
            // env がヒープ型をキャプチャするなら drop_fn を設定（解放時に各キャプチャを release）。
            let cap_tys: Vec<Type> = caps.iter().map(|(_, t)| t.clone()).collect();
            if let Some(dropname) = ensure_drop_fn(m, &cap_tys, &env_ty, 2, "e")? {
                let dp = format!("%t{}", self.fresh());
                self.line(&format!("{} = getelementptr i8, ptr {}, i64 8", dp, env));
                self.line(&format!("store ptr @{}, ptr {}", dropname, dp));
            }
            // 各キャプチャを格納（現スコープから load → env に store）。index 2+i（ヘッダの後）。
            for (i, (cname, cty)) in caps.iter().enumerate() {
                let cv = self.var(cname, cty, m)?;
                let fp = format!("%t{}", self.fresh());
                self.line(&format!(
                    "{} = getelementptr {}, ptr {}, i32 0, i32 {}",
                    fp, env_ty, env, i + 2
                ));
                self.line(&format!("store {} {}, ptr {}", llvm_ty(cty)?, cv, fp));
            }
            env
        };
        // クロージャ {@fnN, env} を組む。
        let c0 = format!("%t{}", self.fresh());
        self.line(&format!(
            "{} = insertvalue {} undef, ptr @{}, 0",
            c0, CLOSURE_TY, fn_name
        ));
        let c1 = format!("%t{}", self.fresh());
        self.line(&format!(
            "{} = insertvalue {} {}, ptr {}, 1",
            c1, CLOSURE_TY, c0, env_operand
        ));
        Ok(c1)
    }
}

// RC 管理するヒープ型か。Future/Socket は i64 ハンドル（ランタイム所有）なので対象外。
fn is_heap(t: &Type) -> bool {
    match t {
        Type::Str | Type::Fun(..) => true,
        Type::Con(name, _) => name != "Future" && name != "Socket",
        _ => false,
    }
}

// ヒープオブジェクトの drop_fn を生成（メモ化）。ヒープ型の構成要素を release してから戻る。
// release（フリー）自体は awa_release が行うので、drop_fn の役目は「子の参照を1つ減らす」こと。
// ヒープ要素が無ければ None（ヘッダの drop は null のまま＝解放するだけ）。
// 構築子ブロック（base=3, prefix="b"）と env（base=2, prefix="e"）の両方で使う。
// 同じ要素型でもレイアウト（struct_ty/base）が違えば drop ロジックも違うので prefix で名前を分ける。
fn ensure_drop_fn(
    m: &mut Module,
    tys: &[Type],
    struct_ty: &str,
    base: usize,
    prefix: &str,
) -> Result<Option<String>> {
    if !tys.iter().any(is_heap) {
        return Ok(None);
    }
    let key: String = tys.iter().map(mangle_ty).collect::<Vec<_>>().join("_");
    let name = format!("drop${}${}", prefix, key);
    if m.drops.contains(&name) {
        return Ok(Some(name));
    }
    m.drops.insert(name.clone());
    let mut g = Gen::new();
    for (i, ty) in tys.iter().enumerate() {
        if !is_heap(ty) {
            continue;
        }
        let fp = format!("%t{}", g.fresh());
        g.line(&format!(
            "{} = getelementptr {}, ptr %o, i32 0, i32 {}",
            fp,
            struct_ty,
            base + i
        ));
        let fv = format!("%t{}", g.fresh());
        g.line(&format!("{} = load {}, ptr {}", fv, llvm_ty(ty)?, fp));
        g.release(&fv, ty);
    }
    g.line("ret void");
    m.funcs
        .push_str(&format!("define void @{}(ptr %o) {{\n{}}}\n\n", name, g.out));
    Ok(Some(name))
}

// 1 つの関数定義を生成して module.funcs に追記する。
// シグネチャ: define <ret> @name(ptr %env, <params...>)。本体冒頭でキャプチャを env から load。
fn gen_function(
    m: &mut Module,
    name: &str,
    caps: &[(String, Type)],
    params: &[(String, Type)],
    ret: &Type,
    body: &TExpr,
) -> Result<()> {
    let ret_ll = llvm_ty(ret)?;
    let mut g = Gen::new();
    // 引数ヘッダ（先頭は env）。
    let mut header = vec!["ptr %env".to_string()];
    for (i, (_, pty)) in params.iter().enumerate() {
        header.push(format!("{} %arg{}", llvm_ty(pty)?, i));
    }
    // キャプチャを env から load してローカルに束縛。
    if !caps.is_empty() {
        let env_ty = env_struct_ty(caps)?;
        for (i, (cname, cty)) in caps.iter().enumerate() {
            let fp = format!("%t{}", g.fresh());
            g.line(&format!(
                "{} = getelementptr {}, ptr %env, i32 0, i32 {}",
                fp, env_ty, i + 2
            ));
            let v = format!("%t{}", g.fresh());
            g.line(&format!("{} = load {}, ptr {}", v, llvm_ty(cty)?, fp));
            // env から借りた参照を関数のローカルとして所有するため retain（epilogue で release）。
            g.retain(&v, cty);
            g.bind_local(cname, &v, cty)?;
        }
    }
    // 引数を束縛。
    for (i, (pname, pty)) in params.iter().enumerate() {
        g.bind_local(pname, &format!("%arg{}", i), pty)?;
    }
    let result = g.expr(body, m)?;
    // 関数は引数・キャプチャを consume する → 戻り値を作った後に release（戻り値は別の +1）。
    g.release_scope_since(&HashMap::new())?;
    g.line(&format!("ret {} {}", ret_ll, result));
    m.funcs.push_str(&format!(
        "define {} {}({}) {{\n{}}}\n\n",
        ret_ll,
        sym(name),
        header.join(", "),
        g.out
    ));
    Ok(())
}

// 本体の自由変数（params と内側で束縛される名前を除く）。決定的な順序で返す。
fn free_vars(params: &[String], body: &TExpr) -> Vec<String> {
    let mut bound: HashSet<String> = params.iter().cloned().collect();
    let mut free: Vec<String> = Vec::new();
    fv(body, &mut bound, &mut free);
    free.sort();
    free.dedup();
    free
}

fn fv(e: &TExpr, bound: &HashSet<String>, free: &mut Vec<String>) {
    match &e.kind {
        TExprKind::Int(_)
        | TExprKind::Long(_)
        | TExprKind::Float(_)
        | TExprKind::Str(_)
        | TExprKind::Bool(_) => {}
        TExprKind::Var(n) => {
            if !bound.contains(n) {
                free.push(n.clone());
            }
        }
        TExprKind::Neg(x) => fv(x, bound, free),
        TExprKind::BinOp(l, _, r) => {
            fv(l, bound, free);
            fv(r, bound, free);
        }
        TExprKind::If(c, t, e2) => {
            fv(c, bound, free);
            fv(t, bound, free);
            fv(e2, bound, free);
        }
        TExprKind::Lambda(ps, b) => {
            let mut b2 = bound.clone();
            for p in ps {
                b2.insert(p.clone());
            }
            fv(b, &b2, free);
        }
        TExprKind::Call(c, args) => {
            fv(c, bound, free);
            for a in args {
                fv(a, bound, free);
            }
        }
        TExprKind::Block(stmts) => {
            let mut b2 = bound.clone();
            for s in stmts {
                match s {
                    TStmt::Let(n, rhs) => {
                        fv(rhs, &b2, free);
                        b2.insert(n.clone());
                    }
                    TStmt::Expr(e2) => fv(e2, &b2, free),
                    TStmt::Type(_) | TStmt::Impl(_) => {}
                }
            }
        }
        TExprKind::Case(subj, arms) => {
            fv(subj, bound, free);
            for (pat, body) in arms {
                let mut b2 = bound.clone();
                pattern_vars(pat, &mut b2);
                fv(body, &b2, free);
            }
        }
    }
}

fn pattern_vars(p: &crate::ast::Pattern, out: &mut HashSet<String>) {
    use crate::ast::Pattern;
    match p {
        Pattern::Var(n) => {
            out.insert(n.clone());
        }
        Pattern::Ctor(_, subs) | Pattern::Or(subs) => {
            for s in subs {
                pattern_vars(s, out);
            }
        }
        _ => {}
    }
}

// プログラム全体 → LLVM IR テキスト。
pub fn compile_program(stmts: &[TStmt]) -> Result<String> {
    let mut globals: HashMap<String, GlobalFn> = HashMap::new();
    for s in stmts {
        if let TStmt::Let(name, te) = s {
            if let TExprKind::Lambda(param_names, body) = &te.kind {
                let Type::Fun(params, ret) = &te.ty else {
                    return unsupported("関数の型が解決できない");
                };
                globals.insert(
                    name.clone(),
                    GlobalFn {
                        param_names: param_names.clone(),
                        param_tys: params.clone(),
                        ret: (**ret).clone(),
                        body: (**body).clone(),
                    },
                );
            }
        }
        // メソッドは self を第一引数に取るトップレベル関数として登録する。
        if let TStmt::Impl(methods) = s {
            for meth in methods {
                globals.insert(
                    meth.mangled.clone(),
                    GlobalFn {
                        param_names: meth.params.clone(),
                        param_tys: meth.param_tys.clone(),
                        ret: meth.ret.clone(),
                        body: meth.body.clone(),
                    },
                );
            }
        }
    }

    // 構築子レジストリ（タグは型ごとに宣言順 0,1,...）。
    let mut ctors: HashMap<String, CtorInfo> = HashMap::new();
    for s in stmts {
        if let TStmt::Type(def) = s {
            for (tag, v) in def.variants.iter().enumerate() {
                ctors.insert(
                    v.name.clone(),
                    CtorInfo {
                        tag: tag as u32,
                        arity: v.fields.len(),
                        type_params: def.params.clone(),
                        field_texprs: v.fields.clone(),
                    },
                );
            }
        }
    }

    // トップレベルの値 let（関数でない）→ LLVM グローバル変数。関数本体からも参照可能。
    let mut value_globals: HashMap<String, Type> = HashMap::new();
    for s in stmts {
        if let TStmt::Let(name, te) = s {
            if !matches!(te.kind, TExprKind::Lambda(..)) {
                value_globals.insert(name.clone(), te.ty.clone());
            }
        }
    }

    let mut m = Module {
        funcs: String::new(),
        fn_ctr: 0,
        globals,
        value_globals,
        instances: HashSet::new(),
        ctors,
        strings: String::new(),
        str_ctr: 0,
        drops: HashSet::new(),
    };

    // 単相なトップレベル関数だけを @name で先に出力する。
    // 総称関数は使用箇所の具体型ごとに ensure_instance が遅延発行する。
    let mono: Vec<String> = m
        .globals
        .iter()
        .filter(|(_, gf)| !gf.is_generic())
        .map(|(n, _)| n.clone())
        .collect();
    for name in mono {
        let gf = m.globals.get(&name).unwrap().clone();
        let typed_params: Vec<(String, Type)> = gf
            .param_names
            .iter()
            .cloned()
            .zip(gf.param_tys.iter().cloned())
            .collect();
        gen_function(&mut m, &name, &[], &typed_params, &gf.ret, &gf.body)?;
    }

    // main。
    let main = compile_main(stmts, &mut m)?;

    let mut module = String::new();
    // ランタイム宣言。
    module.push_str("declare i32 @printf(ptr, ...)\n");
    module.push_str("declare ptr @malloc(i64)\n");
    module.push_str("declare i64 @strlen(ptr)\n");
    module.push_str("declare ptr @memcpy(ptr, ptr, i64)\n");
    module.push_str("declare i32 @strcmp(ptr, ptr)\n");
    module.push_str("declare i32 @snprintf(ptr, i64, ptr, ...)\n");
    module.push_str("declare void @exit(i32)\n");
    module.push_str("declare void @free(ptr)\n");
    // 非同期イベントループ（C ランタイム async_runtime.c が定義、build 時にリンク）。
    module.push_str("declare i64 @awa_future_pure(i64)\n");
    module.push_str("declare i64 @awa_future_delay(i64, i64)\n");
    module.push_str("declare i64 @awa_future_flatmap(i64, ptr, ptr)\n");
    module.push_str("declare i64 @awa_future_map(i64, ptr, ptr)\n");
    module.push_str("declare i64 @awa_future_run(i64)\n");
    // OS 連携: 同期 I/O（async_runtime.c が定義）。
    module.push_str("declare void @awa_print(ptr)\n");
    module.push_str("declare ptr @awa_read_line()\n");
    module.push_str("declare ptr @awa_read_file(ptr)\n");
    module.push_str("declare void @awa_write_file(ptr, ptr)\n");
    module.push_str("declare i64 @awa_now()\n");
    // OS 連携: ソケット非同期（async_runtime.c が定義）。
    module.push_str("declare i64 @awa_tcp_listen(i64)\n");
    module.push_str("declare i64 @awa_sock_port(i64)\n");
    module.push_str("declare i64 @awa_accept(i64)\n");
    module.push_str("declare i64 @awa_sock_read(i64)\n");
    module.push_str("declare i64 @awa_sock_write(i64, ptr)\n");
    module.push_str("declare void @awa_sock_close(i64)\n");
    module.push_str("declare i64 @awa_tcp_connect(ptr, i64)\n");
    // 生存ヒープオブジェクト数（リーク検出用）。init_header と awa_make_str で ++、awa_release で --。
    module.push_str("@awa_live = global i64 0\n");
    // 書式・定数文字列。
    module.push_str("@.di = private unnamed_addr constant [6 x i8] c\"%lld\\0A\\00\"\n");
    module.push_str("@.df = private unnamed_addr constant [4 x i8] c\"%g\\0A\\00\"\n");
    module.push_str("@.ds = private unnamed_addr constant [4 x i8] c\"%s\\0A\\00\"\n");
    module.push_str("@.fi = private unnamed_addr constant [5 x i8] c\"%lld\\00\"\n");
    module.push_str("@.ff = private unnamed_addr constant [3 x i8] c\"%g\\00\"\n");
    module.push_str("@.t = private unnamed_addr constant [5 x i8] c\"true\\00\"\n");
    module.push_str("@.f = private unnamed_addr constant [6 x i8] c\"false\\00\"\n");
    module.push_str("@.dz = private unnamed_addr constant [18 x i8] c\"division by zero\\0A\\00\"\n");
    module.push_str(&m.strings);
    // トップレベル値のグローバル変数（main が store、関数本体が load）。
    let mut vg: Vec<(&String, &Type)> = m.value_globals.iter().collect();
    vg.sort_by(|a, b| a.0.cmp(b.0));
    for (name, ty) in vg {
        module.push_str(&format!("@{} = global {} zeroinitializer\n", name, llvm_ty(ty)?));
    }
    // 参照カウントのランタイム。
    // retain: rc++（null と rc<0 の不滅オブジェクトは無視）。
    module.push_str(
        "\ndefine void @awa_retain(ptr %o) {\n\
         entry:\n\
         \x20 %isn = icmp eq ptr %o, null\n\
         \x20 br i1 %isn, label %done, label %live\n\
         live:\n\
         \x20 %rc = load i64, ptr %o\n\
         \x20 %imm = icmp slt i64 %rc, 0\n\
         \x20 br i1 %imm, label %done, label %inc\n\
         inc:\n\
         \x20 %rc1 = add i64 %rc, 1\n\
         \x20 store i64 %rc1, ptr %o\n\
         \x20 br label %done\n\
         done:\n\
         \x20 ret void\n\
         }\n\n",
    );
    // release: rc--。0 になったら drop_fn（あれば子を release）を呼び、@awa_live を減らして free。
    module.push_str(
        "\ndefine void @awa_release(ptr %o) {\n\
         entry:\n\
         \x20 %isn = icmp eq ptr %o, null\n\
         \x20 br i1 %isn, label %done, label %live\n\
         live:\n\
         \x20 %rc = load i64, ptr %o\n\
         \x20 %imm = icmp slt i64 %rc, 0\n\
         \x20 br i1 %imm, label %done, label %dec\n\
         dec:\n\
         \x20 %rc1 = sub i64 %rc, 1\n\
         \x20 store i64 %rc1, ptr %o\n\
         \x20 %dead = icmp eq i64 %rc1, 0\n\
         \x20 br i1 %dead, label %dofree, label %done\n\
         dofree:\n\
         \x20 %lv = load i64, ptr @awa_live\n\
         \x20 %lv1 = sub i64 %lv, 1\n\
         \x20 store i64 %lv1, ptr @awa_live\n\
         \x20 %dp = getelementptr i8, ptr %o, i64 8\n\
         \x20 %drop = load ptr, ptr %dp\n\
         \x20 %dn = icmp eq ptr %drop, null\n\
         \x20 br i1 %dn, label %fr, label %cd\n\
         cd:\n\
         \x20 call void %drop(ptr %o)\n\
         \x20 br label %fr\n\
         fr:\n\
         \x20 call void @free(ptr %o)\n\
         \x20 br label %done\n\
         done:\n\
         \x20 ret void\n\
         }\n\n",
    );
    // 文字列連結ランタイム。引数は文字列オブジェクト ptr、本体は obj+16。
    // 結果もヘッダ込みで確保し rc=1/drop=null を設定して返す。
    module.push_str(
        "\ndefine ptr @awa_concat(ptr %a, ptr %b) {\n\
         \x20 %ab = getelementptr i8, ptr %a, i64 16\n\
         \x20 %bb = getelementptr i8, ptr %b, i64 16\n\
         \x20 %la = call i64 @strlen(ptr %ab)\n\
         \x20 %lb = call i64 @strlen(ptr %bb)\n\
         \x20 %lab = add i64 %la, %lb\n\
         \x20 %body = add i64 %lab, 1\n\
         \x20 %sz = add i64 %body, 16\n\
         \x20 %obj = call ptr @malloc(i64 %sz)\n\
         \x20 store i64 1, ptr %obj\n\
         \x20 %dp = getelementptr i8, ptr %obj, i64 8\n\
         \x20 store ptr null, ptr %dp\n\
         \x20 %lv = load i64, ptr @awa_live\n\
         \x20 %lv1 = add i64 %lv, 1\n\
         \x20 store i64 %lv1, ptr @awa_live\n\
         \x20 %buf = getelementptr i8, ptr %obj, i64 16\n\
         \x20 %c1 = call ptr @memcpy(ptr %buf, ptr %ab, i64 %la)\n\
         \x20 %tail = getelementptr i8, ptr %buf, i64 %la\n\
         \x20 %lb1 = add i64 %lb, 1\n\
         \x20 %c2 = call ptr @memcpy(ptr %tail, ptr %bb, i64 %lb1)\n\
         \x20 ret ptr %obj\n\
         }\n\n",
    );
    module.push_str(&m.funcs);
    module.push_str(&main);
    Ok(module)
}

fn compile_main(stmts: &[TStmt], m: &mut Module) -> Result<String> {
    let mut g = Gen::new();
    for s in stmts {
        match s {
            TStmt::Let(_, te) if matches!(te.kind, TExprKind::Lambda(..)) => {}
            // トップレベル値はグローバル @name に格納（関数本体からも参照可能）。
            TStmt::Let(name, rhs) => {
                let v = g.expr(rhs, m)?;
                g.line(&format!("store {} {}, ptr @{}", llvm_ty(&rhs.ty)?, v, name));
            }
            TStmt::Expr(e) => {
                let v = g.expr(e, m)?;
                emit_print(&mut g, &v, &e.ty)?;
                g.release(&v, &e.ty); // 表示したら値は破棄
            }
            TStmt::Type(_) | TStmt::Impl(_) => {}
        }
    }
    // プログラム終了前に、トップレベル値（グローバル変数が保持する +1）を release。
    // これで RC が正しければ生存数 @awa_live は 0 になる。
    let mut vg: Vec<(String, Type)> = m
        .value_globals
        .iter()
        .map(|(n, t)| (n.clone(), t.clone()))
        .collect();
    vg.sort_by(|a, b| a.0.cmp(&b.0));
    for (name, ty) in vg {
        if is_heap(&ty) {
            let v = format!("%t{}", g.fresh());
            g.line(&format!("{} = load {}, ptr @{}", v, llvm_ty(&ty)?, name));
            g.release(&v, &ty);
        }
    }
    // 終了コード = 残存ヒープ数（リーク検出: 正常時 0）。
    let lc = format!("%t{}", g.fresh());
    g.line(&format!("{} = load i64, ptr @awa_live", lc));
    let ec = format!("%t{}", g.fresh());
    g.line(&format!("{} = trunc i64 {} to i32", ec, lc));
    g.line(&format!("ret i32 {}", ec));
    Ok(format!("define i32 @main() {{\n{}}}\n", g.out))
}

// ネイティブランタイム（同期 I/O ＋ 非同期イベントループ）の C ソース。
// 実体は src/runtime.c。`awa build` 時に clang で .ll と一緒にリンクする。
pub const ASYNC_RUNTIME_C: &str = include_str!("runtime.c");

fn emit_print(g: &mut Gen, operand: &str, ty: &Type) -> Result<()> {
    match ty {
        Type::Int | Type::Long => {
            let r = g.fresh();
            g.line(&format!(
                "%p{} = call i32 (ptr, ...) @printf(ptr @.di, i64 {})",
                r, operand
            ));
        }
        Type::Float => {
            let r = g.fresh();
            g.line(&format!(
                "%p{} = call i32 (ptr, ...) @printf(ptr @.df, double {})",
                r, operand
            ));
        }
        Type::Bool => {
            let s = g.fresh();
            g.line(&format!("%s{} = select i1 {}, ptr @.t, ptr @.f", s, operand));
            let r = g.fresh();
            g.line(&format!(
                "%p{} = call i32 (ptr, ...) @printf(ptr @.ds, ptr %s{})",
                r, s
            ));
        }
        Type::Str => {
            let body = g.str_body(operand);
            let r = g.fresh();
            g.line(&format!(
                "%p{} = call i32 (ptr, ...) @printf(ptr @.ds, ptr {})",
                r, body
            ));
        }
        // Unit（Nil）は表示しない（print/write_file 等の副作用文の値）。
        Type::Unit => {}
        other => return unsupported(format!("出力できない型 {}", other)),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::infer::Checker;
    use crate::lexer::tokenize;
    use crate::parser::Parser;

    fn try_ir(src: &str) -> Result<String> {
        let tokens = tokenize(src).unwrap();
        let program = Parser::new(tokens).parse_program().unwrap();
        let typed = Checker::new().check_program(&program).unwrap();
        compile_program(&typed)
    }
    fn ir(src: &str) -> String {
        try_ir(src).unwrap()
    }

    #[test]
    fn 関数と呼び出しと算術を出力する() {
        let out = ir("let f = fn(x: Int) -> Int { x + 1 }; f(2)");
        assert!(out.contains("define i64 @f(ptr %env, i64 %arg0)"));
        assert!(out.contains("add i64"));
        assert!(out.contains("call i64 @f(ptr null"));
        assert!(out.contains("define i32 @main()"));
    }

    #[test]
    fn 再帰関数とifを出力する() {
        let out = ir("let fact = fn(n: Int) -> Int { if n == 0 { 1 } else { n * fact(n - 1) } }; fact(5)");
        assert!(out.contains("define i64 @fact"));
        assert!(out.contains("icmp eq i64"));
        assert!(out.contains("mul i64"));
    }

    #[test]
    fn クロージャはmallocとキャプチャを出力する() {
        let out = ir("let mk = fn(n: Int) { fn(x: Int) -> Int { x + n } }; 0");
        assert!(out.contains("@malloc"));
        assert!(out.contains("insertvalue"));
    }

    #[test]
    fn 文字列の連結を出力する() {
        let out = ir(r#""a" <> "b""#);
        assert!(out.contains("@awa_concat"));
        assert!(out.contains("constant")); // 文字列リテラルのグローバル
    }

    #[test]
    fn adtの構築とcaseを出力する() {
        let out = ir(
            "type Opt(a) { Some(a); None }; \
             let f = fn(o: Opt(Int), d: Int) -> Int { case o { Some(x) -> x; None -> d } }; \
             f(Some(1), 0)",
        );
        assert!(out.contains("@malloc")); // 構築でヒープ確保
        assert!(out.contains("load i32")); // case で tag を読む
        assert!(out.contains("unreachable")); // 網羅後のフォールスルー
    }
}
