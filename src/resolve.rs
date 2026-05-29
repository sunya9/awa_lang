// ===== モジュール名前解決（Phase 10）=====
// パース済みの各モジュール（= 各ファイル）を 1 つのフラットなプログラムへ「畳み込む」段。
// やることは untyped AST 上のリネーム:
//   - そのモジュールのトップレベル定義（値・型・構築子）と、それへの参照を、
//     モジュール接頭辞付きの完全修飾名にする（エントリモジュールの接頭辞は ""＝従来どおり素の名前）。
//   - 修飾参照（値 `mod.f(x)`、構築子 `mod.Some(x)`/`mod.None`、型 `mod.Color`）を、
//     対象モジュールの完全修飾名へ展開する。公開（pub）されていなければエラー。
// こうすると後段（型検査・eval・codegen）はモジュールを一切知らずに済む
//   ＝「名前が util.double のような長い文字列になっただけ」と見なせる。
use std::collections::{HashMap, HashSet};

use crate::ast::{Expr, ExprKind, Import, Pattern, Stmt, TypeExpr};
use crate::error::{AwaError, LocatedError};

// ローダが用意する、解決前の 1 モジュール。
#[derive(Debug)]
pub struct LoadedModule {
    pub path: Vec<String>,       // モジュールパス（["app","util"]）
    pub prefix: String,          // シンボル接頭辞（エントリは ""、他は "app.util."）
    pub imports: Vec<Import>,
    pub exports: HashSet<String>, // pub が付いたトップレベル名（値名・型名）
    pub stmts: Vec<Stmt>,
    pub source: String,          // エラー報告用の元ソース
}

// あるモジュールが外部公開する全名前（pub 値・pub 型・pub 型の構築子）。
fn effective_exports(m: &LoadedModule) -> HashSet<String> {
    let mut eff = m.exports.clone();
    for s in &m.stmts {
        if let Stmt::Type(def) = s {
            if m.exports.contains(&def.name) {
                // pub 型はその構築子もまとめて公開する（Gleam と同じ）。
                for v in &def.variants {
                    eff.insert(v.name.clone());
                }
            }
        }
    }
    eff
}

// 依存順（依存先が先）に並んだモジュール列を、完全修飾された 1 つの Stmt 列へ畳み込む。
pub fn resolve(modules: &[LoadedModule]) -> Result<Vec<Stmt>, LocatedError> {
    // モジュールパス文字列 → (接頭辞, 公開名集合)。
    let mut registry: HashMap<String, (String, HashSet<String>)> = HashMap::new();
    for m in modules {
        registry.insert(m.path.join("/"), (m.prefix.clone(), effective_exports(m)));
    }

    let mut out = Vec::new();
    for m in modules {
        // import 修飾子 → (対象接頭辞, 対象公開名集合)。
        let mut imap: HashMap<String, (String, HashSet<String>)> = HashMap::new();
        for imp in &m.imports {
            let key = imp.path.join("/");
            let entry = registry.get(&key).cloned().ok_or_else(|| {
                LocatedError::from(AwaError::Unsupported(format!("モジュールが見つかりません: {}", key)))
            })?;
            imap.insert(imp.qualifier().to_string(), entry);
        }
        // このモジュールのトップレベル値名・型名・構築子名（内部参照を修飾するため）。
        let mut topdefs = HashSet::new();
        let mut types = HashSet::new();
        let mut ctors = HashSet::new();
        for s in &m.stmts {
            match s {
                Stmt::Let(name, _, _) => {
                    topdefs.insert(name.clone());
                }
                Stmt::Type(def) => {
                    types.insert(def.name.clone());
                    for v in &def.variants {
                        ctors.insert(v.name.clone());
                    }
                }
                _ => {}
            }
        }
        let r = Resolver {
            prefix: &m.prefix,
            topdefs: &topdefs,
            types: &types,
            ctors: &ctors,
            imap: &imap,
        };
        for s in &m.stmts {
            out.push(r.stmt(s)?);
        }
    }
    Ok(out)
}

struct Resolver<'a> {
    prefix: &'a str,
    topdefs: &'a HashSet<String>, // 値 let 名
    types: &'a HashSet<String>,   // 型名
    ctors: &'a HashSet<String>,   // 構築子名
    imap: &'a HashMap<String, (String, HashSet<String>)>,
}

impl Resolver<'_> {
    // 自モジュールのトップレベル名に接頭辞を付ける。
    fn qualify(&self, name: &str) -> String {
        format!("{}{}", self.prefix, name)
    }

    // "alias.Name" を対象モジュールの完全修飾名へ。alias が import 済みなら公開検査も行う。
    // alias が未 import なら None（呼び出し側でそのまま残し、型検査側の未定義エラーに委ねる）。
    fn resolve_qualified(&self, alias: &str, name: &str) -> Result<Option<String>, LocatedError> {
        match self.imap.get(alias) {
            Some((tprefix, texports)) => {
                if !texports.contains(name) {
                    return Err(LocatedError::from(AwaError::NotExported {
                        module: alias.to_string(),
                        name: name.to_string(),
                    }));
                }
                Ok(Some(format!("{}{}", tprefix, name)))
            }
            None => Ok(None),
        }
    }

    fn stmt(&self, s: &Stmt) -> Result<Stmt, LocatedError> {
        match s {
            Stmt::Let(name, ann, e) => {
                let ann = self.opt_type(ann)?;
                let body = self.expr(e, &HashSet::new())?;
                Ok(Stmt::Let(self.qualify(name), ann, body))
            }
            Stmt::Expr(e) => Ok(Stmt::Expr(self.expr(e, &HashSet::new())?)),
            // 型定義: 型名・構築子名・フィールド型をすべて修飾する。
            Stmt::Type(def) => {
                let mut def = def.clone();
                def.name = self.qualify(&def.name);
                for v in &mut def.variants {
                    v.name = self.qualify(&v.name);
                    v.fields = v
                        .fields
                        .iter()
                        .map(|t| self.type_expr(t))
                        .collect::<Result<_, _>>()?;
                }
                Ok(Stmt::Type(def))
            }
            Stmt::Impl(b) => {
                let mut b = b.clone();
                // 自モジュールの型への impl なら型名も修飾（メソッド解決の型頭を一致させる）。
                if self.types.contains(&b.type_name) {
                    b.type_name = self.qualify(&b.type_name);
                }
                for meth in &mut b.methods {
                    let locals: HashSet<String> =
                        meth.params.iter().map(|(n, _)| n.clone()).collect();
                    meth.body = self.expr(&meth.body, &locals)?;
                }
                Ok(Stmt::Impl(b))
            }
        }
    }

    fn opt_type(&self, ann: &Option<TypeExpr>) -> Result<Option<TypeExpr>, LocatedError> {
        match ann {
            Some(t) => Ok(Some(self.type_expr(t)?)),
            None => Ok(None),
        }
    }

    // 型式の名前を修飾する。`alias.Color` は対象モジュールへ、自モジュールの型は接頭辞付きへ。
    fn type_expr(&self, t: &TypeExpr) -> Result<TypeExpr, LocatedError> {
        match t {
            TypeExpr::Var(v) => Ok(TypeExpr::Var(v.clone())),
            TypeExpr::Fun(ps, r) => {
                let ps = ps.iter().map(|p| self.type_expr(p)).collect::<Result<_, _>>()?;
                Ok(TypeExpr::Fun(ps, Box::new(self.type_expr(r)?)))
            }
            TypeExpr::Con(name, args) => {
                let args = args.iter().map(|a| self.type_expr(a)).collect::<Result<_, _>>()?;
                let newname = if let Some((alias, rest)) = name.split_once('.') {
                    self.resolve_qualified(alias, rest)?.unwrap_or_else(|| name.clone())
                } else if self.types.contains(name) {
                    self.qualify(name)
                } else {
                    name.clone() // 組込型（Int/String など）・型変数 → そのまま
                };
                Ok(TypeExpr::Con(newname, args))
            }
        }
    }

    // パターンの構築子名を修飾する。
    fn pattern(&self, p: &Pattern) -> Result<Pattern, LocatedError> {
        match p {
            Pattern::Ctor(name, subs) => {
                let subs = subs.iter().map(|s| self.pattern(s)).collect::<Result<_, _>>()?;
                Ok(Pattern::Ctor(self.ctor_name(name)?, subs))
            }
            Pattern::Or(alts) => {
                let alts = alts.iter().map(|a| self.pattern(a)).collect::<Result<_, _>>()?;
                Ok(Pattern::Or(alts))
            }
            other => Ok(other.clone()),
        }
    }

    // 構築子名を修飾する（`alias.Ctor` は対象へ、自モジュールの構築子は接頭辞付きへ）。
    fn ctor_name(&self, name: &str) -> Result<String, LocatedError> {
        if let Some((alias, rest)) = name.split_once('.') {
            Ok(self.resolve_qualified(alias, rest)?.unwrap_or_else(|| name.to_string()))
        } else if self.ctors.contains(name) {
            Ok(self.qualify(name))
        } else {
            Ok(name.to_string()) // 組込構築子（Nil 等）・未知 → そのまま
        }
    }

    fn expr(&self, e: &Expr, locals: &HashSet<String>) -> Result<Expr, LocatedError> {
        let kind = match &e.kind {
            ExprKind::Int(_)
            | ExprKind::Long(_)
            | ExprKind::Float(_)
            | ExprKind::Str(_)
            | ExprKind::Bool(_) => e.kind.clone(),
            ExprKind::Var(name) => {
                if locals.contains(name) {
                    ExprKind::Var(name.clone()) // ローカル変数
                } else if self.topdefs.contains(name) || self.ctors.contains(name) {
                    ExprKind::Var(self.qualify(name)) // 自モジュールの値 or 構築子
                } else {
                    ExprKind::Var(name.clone()) // 組込・未知
                }
            }
            ExprKind::Neg(x) => ExprKind::Neg(Box::new(self.expr(x, locals)?)),
            ExprKind::BinOp(l, op, r) => ExprKind::BinOp(
                Box::new(self.expr(l, locals)?),
                op.clone(),
                Box::new(self.expr(r, locals)?),
            ),
            ExprKind::If(c, t, e2) => ExprKind::If(
                Box::new(self.expr(c, locals)?),
                Box::new(self.expr(t, locals)?),
                Box::new(self.expr(e2, locals)?),
            ),
            ExprKind::Lambda(params, ret, body) => {
                let params = params
                    .iter()
                    .map(|(n, a)| Ok::<_, LocatedError>((n.clone(), self.opt_type(a)?)))
                    .collect::<Result<Vec<_>, _>>()?;
                let ret = self.opt_type(ret)?;
                let mut inner = locals.clone();
                for (p, _) in &params {
                    inner.insert(p.clone());
                }
                ExprKind::Lambda(params, ret, Box::new(self.expr(body, &inner)?))
            }
            ExprKind::Call(callee, args) => ExprKind::Call(
                Box::new(self.expr(callee, locals)?),
                self.exprs(args, locals)?,
            ),
            ExprKind::Block(stmts) => ExprKind::Block(self.block(stmts, locals)?),
            ExprKind::Case(subj, arms) => {
                let subj = Box::new(self.expr(subj, locals)?);
                let mut out = Vec::with_capacity(arms.len());
                for (pat, body) in arms {
                    let pat2 = self.pattern(pat)?;
                    let mut inner = locals.clone();
                    pattern_vars(pat, &mut inner);
                    out.push((pat2, self.expr(body, &inner)?));
                }
                ExprKind::Case(subj, out)
            }
            // `mod.f(args)`（括弧あり）。レシーバが import 修飾子なら修飾名での呼び出しへ展開。
            ExprKind::MethodCall(recv, method, args) => {
                if let ExprKind::Var(q) = &recv.kind {
                    if !locals.contains(q) {
                        if let Some(qname) = self.resolve_qualified(q, method)? {
                            let callee = Expr {
                                kind: ExprKind::Var(qname),
                                span: recv.span,
                            };
                            return Ok(Expr {
                                kind: ExprKind::Call(Box::new(callee), self.exprs(args, locals)?),
                                span: e.span,
                            });
                        }
                    }
                }
                ExprKind::MethodCall(
                    Box::new(self.expr(recv, locals)?),
                    method.clone(),
                    self.exprs(args, locals)?,
                )
            }
            // `mod.member`（括弧なし）= 修飾アクセス。モジュール参照なら Var("mod.member") へ。
            // 値関数の参照（`util.double` を値として渡す）と修飾ヌル構築子（`util.None`）の両方を担う。
            ExprKind::Field(recv, member) => {
                if let ExprKind::Var(q) = &recv.kind {
                    if !locals.contains(q) {
                        if let Some(qname) = self.resolve_qualified(q, member)? {
                            return Ok(Expr {
                                kind: ExprKind::Var(qname),
                                span: e.span,
                            });
                        }
                    }
                }
                // モジュール参照でない（= 未 import 等）。型検査側で UnknownModule になる。
                ExprKind::Field(Box::new(self.expr(recv, locals)?), member.clone())
            }
        };
        Ok(Expr { kind, span: e.span })
    }

    fn exprs(&self, args: &[Expr], locals: &HashSet<String>) -> Result<Vec<Expr>, LocatedError> {
        args.iter().map(|a| self.expr(a, locals)).collect()
    }

    // ブロック内の文。block-local の let 名はローカルとして以降の参照を素のままにする。
    fn block(&self, stmts: &[Stmt], locals: &HashSet<String>) -> Result<Vec<Stmt>, LocatedError> {
        let mut inner = locals.clone();
        let mut out = Vec::with_capacity(stmts.len());
        for s in stmts {
            match s {
                Stmt::Let(name, ann, e) => {
                    // letrec: 自己再帰のため名前を先に可視化してから本体を解決。
                    inner.insert(name.clone());
                    let ann = self.opt_type(ann)?;
                    let body = self.expr(e, &inner)?;
                    out.push(Stmt::Let(name.clone(), ann, body));
                }
                Stmt::Expr(e) => out.push(Stmt::Expr(self.expr(e, &inner)?)),
                // ブロック内の型/impl は稀。M2 ではそのまま（修飾しない＝モジュール内限定）。
                Stmt::Type(d) => out.push(Stmt::Type(d.clone())),
                Stmt::Impl(b) => out.push(Stmt::Impl(b.clone())),
            }
        }
        Ok(out)
    }
}

// パターンが束縛する変数名を集める（case の腕でローカルに加えるため）。
fn pattern_vars(p: &Pattern, out: &mut HashSet<String>) {
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
