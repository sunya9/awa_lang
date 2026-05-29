// ===== モジュール名前解決（Phase 10）=====
// パース済みの各モジュール（= 各ファイル）を 1 つのフラットなプログラムへ「畳み込む」段。
// やることは untyped AST 上のリネーム:
//   - そのモジュールのトップレベル定義名と、それへの参照を、モジュール接頭辞付きの完全修飾名にする
//     （エントリモジュールの接頭辞は ""＝従来どおり素の名前で、単一ファイルは無改変）。
//   - 修飾参照 `mod.name`（パーサ上は MethodCall(Var(mod), name, args)）を、対象モジュールの
//     完全修飾名への通常 Call へ展開する。公開（pub）されていなければエラー。
// こうすると後段（型検査・eval・codegen）はモジュールを一切知らずに済む
//   ＝「名前が app.util.double のような長い文字列になっただけ」と見なせる。
use std::collections::{HashMap, HashSet};

use crate::ast::{Expr, ExprKind, Import, Pattern, Stmt};
use crate::error::{AwaError, LocatedError};

// ローダが用意する、解決前の 1 モジュール。
#[derive(Debug)]
pub struct LoadedModule {
    pub path: Vec<String>,       // モジュールパス（["app","util"]）
    pub prefix: String,          // シンボル接頭辞（エントリは ""、他は "app.util."）
    pub imports: Vec<Import>,
    pub exports: HashSet<String>, // pub 公開された名前
    pub stmts: Vec<Stmt>,
    pub source: String,          // エラー報告用の元ソース
}

// 依存順（依存先が先）に並んだモジュール列を、完全修飾された 1 つの Stmt 列へ畳み込む。
pub fn resolve(modules: &[LoadedModule]) -> Result<Vec<Stmt>, LocatedError> {
    // モジュールパス文字列 → (接頭辞, 公開名集合)。
    let mut registry: HashMap<String, (String, HashSet<String>)> = HashMap::new();
    for m in modules {
        registry.insert(m.path.join("/"), (m.prefix.clone(), m.exports.clone()));
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
        // このモジュールのトップレベル値定義名（内部参照を修飾するため）。
        let topdefs: HashSet<String> = m
            .stmts
            .iter()
            .filter_map(top_value_name)
            .map(|s| s.to_string())
            .collect();
        let r = Resolver {
            prefix: &m.prefix,
            topdefs: &topdefs,
            imap: &imap,
        };
        for s in &m.stmts {
            out.push(r.stmt(s)?);
        }
    }
    Ok(out)
}

// トップレベルの「値」定義名（M1 で修飾対象にするのは値 let のみ。型は M-mod-2）。
fn top_value_name(stmt: &Stmt) -> Option<&str> {
    match stmt {
        Stmt::Let(name, _, _) => Some(name),
        _ => None,
    }
}

struct Resolver<'a> {
    prefix: &'a str,
    topdefs: &'a HashSet<String>,
    imap: &'a HashMap<String, (String, HashSet<String>)>,
}

impl Resolver<'_> {
    // トップレベルの修飾名（接頭辞付き）。
    fn qualify(&self, name: &str) -> String {
        format!("{}{}", self.prefix, name)
    }

    fn stmt(&self, s: &Stmt) -> Result<Stmt, LocatedError> {
        match s {
            Stmt::Let(name, ann, e) => {
                // letrec: 定義名は先に「修飾済みトップレベル」として可視（自己再帰のため）。
                let body = self.expr(e, &HashSet::new())?;
                Ok(Stmt::Let(self.qualify(name), ann.clone(), body))
            }
            Stmt::Expr(e) => Ok(Stmt::Expr(self.expr(e, &HashSet::new())?)),
            // M1: 型・構築子はモジュール内のみ（素の名前のまま）。越境は M-mod-2。
            Stmt::Type(d) => Ok(Stmt::Type(d.clone())),
            Stmt::Impl(b) => {
                let mut b = b.clone();
                for meth in &mut b.methods {
                    let locals: HashSet<String> =
                        meth.params.iter().map(|(n, _)| n.clone()).collect();
                    meth.body = self.expr(&meth.body, &locals)?;
                }
                Ok(Stmt::Impl(b))
            }
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
                    ExprKind::Var(name.clone()) // ローカル変数は素のまま
                } else if self.topdefs.contains(name) {
                    ExprKind::Var(self.qualify(name)) // 自モジュールのトップレベル
                } else {
                    ExprKind::Var(name.clone()) // 組込・構築子・未知 → 触らない
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
                let mut inner = locals.clone();
                for (p, _) in params {
                    inner.insert(p.clone());
                }
                ExprKind::Lambda(params.clone(), ret.clone(), Box::new(self.expr(body, &inner)?))
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
                    let mut inner = locals.clone();
                    pattern_vars(pat, &mut inner);
                    out.push((pat.clone(), self.expr(body, &inner)?));
                }
                ExprKind::Case(subj, out)
            }
            // メソッド呼び出し。レシーバが import 修飾子なら「修飾名での呼び出し」へ展開。
            ExprKind::MethodCall(recv, method, args) => {
                if let ExprKind::Var(q) = &recv.kind {
                    if !locals.contains(q) {
                        if let Some((tprefix, texports)) = self.imap.get(q) {
                            if !texports.contains(method) {
                                return Err(LocatedError::at(
                                    AwaError::NotExported {
                                        module: q.clone(),
                                        name: method.clone(),
                                    },
                                    e.span,
                                ));
                            }
                            // mod.name(args) → 通常の Call(Var("接頭辞.name"), args)。
                            let callee = Expr {
                                kind: ExprKind::Var(format!("{}{}", tprefix, method)),
                                span: recv.span,
                            };
                            return Ok(Expr {
                                kind: ExprKind::Call(Box::new(callee), self.exprs(args, locals)?),
                                span: e.span,
                            });
                        }
                    }
                }
                // 通常のメソッド呼び出し（レシーバの具体型でチェッカが解決）。
                ExprKind::MethodCall(
                    Box::new(self.expr(recv, locals)?),
                    method.clone(),
                    self.exprs(args, locals)?,
                )
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
                    let body = self.expr(e, &inner)?;
                    out.push(Stmt::Let(name.clone(), ann.clone(), body));
                }
                Stmt::Expr(e) => out.push(Stmt::Expr(self.expr(e, &inner)?)),
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
