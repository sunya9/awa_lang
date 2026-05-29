use std::collections::HashMap;

use crate::ast::{Expr, ExprKind, Pattern, Stmt, TypeDef};
use crate::error::{AwaError, LocatedError};
use crate::span::Span;

// ===== 網羅性・到達不能チェック（Phase 3b）=====
// Maranget "Warnings for pattern matching" の usefulness アルゴリズムを、awa の
// 限定パターン（_ / 変数 / Bool / Num / 構築子）向けに実装したもの。型推論とは独立した
// 構文パスで、型検査が通った後に case 式を走査して以下を静的に検出する:
//   - 網羅性: どの値にも当たらない穴があれば NonExhaustiveMatch（反例パターンつき）
//   - 到達不能: 上の腕に覆われて決して当たらない腕があれば UnreachablePattern

// 構築子シグネチャのレジストリ。完全シグネチャ判定（全兄弟が揃っているか）に使う。
#[derive(Default, Clone)]
pub struct Sigs {
    ctor_type: HashMap<String, String>, // 構築子名 → 所属型名
    variants: HashMap<String, Vec<(String, usize)>>, // 型名 → [(構築子名, arity)]
}

impl Sigs {
    // 型宣言を登録する（構築子名・型名・arity のみ。型推論は不要）。
    pub fn add(&mut self, def: &TypeDef) {
        let vs: Vec<(String, usize)> = def
            .variants
            .iter()
            .map(|v| (v.name.clone(), v.fields.len()))
            .collect();
        for (name, _) in &vs {
            self.ctor_type.insert(name.clone(), def.name.clone());
        }
        self.variants.insert(def.name.clone(), vs);
    }

    // 構築子名の所属型の全構築子（不明なら None＝ブロック外の型など）。
    fn siblings(&self, name: &str) -> Option<&[(String, usize)]> {
        let ty = self.ctor_type.get(name)?;
        self.variants.get(ty).map(|v| v.as_slice())
    }
}

// パターンの「頭」＝どの構築子か。ワイルドカード/変数は頭を持たない（None）。
#[derive(Clone, PartialEq)]
enum Head {
    Ctor(String),
    Bool(bool),
    Int(i64),
    Str(String),
}

fn head_of(p: &Pattern) -> Option<Head> {
    match p {
        Pattern::Wildcard | Pattern::Var(_) => None,
        Pattern::Bool(b) => Some(Head::Bool(*b)),
        Pattern::Int(n) => Some(Head::Int(*n)),
        Pattern::Str(s) => Some(Head::Str(s.clone())),
        Pattern::Ctor(name, _) => Some(Head::Ctor(name.clone())),
        // 行列・クエリは expand で or を除いてから組むので、ここに Or は来ない。
        Pattern::Or(_) => unreachable!("or パターンは expand 済みのはず"),
    }
}

// or パターンを展開して or を含まないパターンの集合にする。
// Some(A | B) → [Some(A), Some(B)]、入れ子は直積（Two(A|B, C|D) → 4 通り）。
fn expand(p: &Pattern) -> Vec<Pattern> {
    match p {
        Pattern::Or(alts) => alts.iter().flat_map(expand).collect(),
        Pattern::Ctor(name, subs) => {
            let expanded: Vec<Vec<Pattern>> = subs.iter().map(expand).collect();
            cartesian(&expanded)
                .into_iter()
                .map(|combo| Pattern::Ctor(name.clone(), combo))
                .collect()
        }
        other => vec![other.clone()],
    }
}

// 各位置の候補リストから全組み合わせ（直積）を作る。
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

// パターンの部分パターン（構築子なら引数列、それ以外は空）。
fn subpatterns(p: &Pattern) -> Vec<Pattern> {
    match p {
        Pattern::Ctor(_, subs) => subs.clone(),
        _ => Vec::new(),
    }
}

fn head_to_pattern(h: &Head, subs: Vec<Pattern>) -> Pattern {
    match h {
        Head::Ctor(name) => Pattern::Ctor(name.clone(), subs),
        Head::Bool(b) => Pattern::Bool(*b),
        Head::Int(n) => Pattern::Int(*n),
        Head::Str(s) => Pattern::Str(s.clone()),
    }
}

// 行列の第 0 列に現れる相異なる頭の集合。
fn column_heads(matrix: &[Vec<Pattern>]) -> Vec<Head> {
    let mut seen = Vec::new();
    for row in matrix {
        if let Some(h) = head_of(&row[0]) {
            if !seen.contains(&h) {
                seen.push(h);
            }
        }
    }
    seen
}

// 第 0 列の型の「完全シグネチャ」を返す（全構築子と arity）。ただし present が
// それを覆い切っている場合のみ Some。覆い切れない / 無限ドメイン(Num) / 不明なら None。
fn complete_signature(present: &[Head], sigs: &Sigs) -> Option<Vec<(Head, usize)>> {
    let full: Vec<(Head, usize)> = match present.first()? {
        Head::Bool(_) => vec![(Head::Bool(true), 0), (Head::Bool(false), 0)],
        // Int/String は無限ドメインなので決して完全にならない（catch-all 必須）。
        Head::Int(_) | Head::Str(_) => return None,
        Head::Ctor(name) => sigs
            .siblings(name)?
            .iter()
            .map(|(n, a)| (Head::Ctor(n.clone()), *a))
            .collect(),
    };
    if full.iter().all(|(h, _)| present.contains(h)) {
        Some(full)
    } else {
        None
    }
}

// 特殊化 S(c, P): 第 0 列が構築子 c の行はその部分パターンを前置、ワイルドカードの行は
// arity 個のワイルドカードを前置、別の構築子の行は捨てる。
fn specialize(matrix: &[Vec<Pattern>], c: &Head, arity: usize) -> Vec<Vec<Pattern>> {
    matrix
        .iter()
        .filter_map(|row| {
            let (first, rest) = row.split_first().unwrap();
            match head_of(first) {
                None => {
                    let mut r = vec![Pattern::Wildcard; arity];
                    r.extend_from_slice(rest);
                    Some(r)
                }
                Some(h) if &h == c => {
                    let mut r = subpatterns(first);
                    r.extend_from_slice(rest);
                    Some(r)
                }
                _ => None,
            }
        })
        .collect()
}

// デフォルト行列 D(P): 第 0 列がワイルドカードの行だけを残し、第 0 列を落とす。
fn default_matrix(matrix: &[Vec<Pattern>]) -> Vec<Vec<Pattern>> {
    matrix
        .iter()
        .filter_map(|row| {
            let (first, rest) = row.split_first().unwrap();
            match head_of(first) {
                None => Some(rest.to_vec()),
                Some(_) => None,
            }
        })
        .collect()
}

// usefulness U(P, q): 行列 P のどの行にも覆われないが q には当たる値が存在するか。
// 到達不能判定に使う（その腕が上の腕の行列に対して useful でなければ到達不能）。
fn useful(matrix: &[Vec<Pattern>], q: &[Pattern], sigs: &Sigs) -> bool {
    let Some((first, rest)) = q.split_first() else {
        // 列が尽きた: 行が 1 つも無ければ（覆われていなければ）useful。
        return matrix.is_empty();
    };
    match head_of(first) {
        // q の先頭が構築子: その構築子で特殊化して進む。
        Some(c) => {
            let subs = subpatterns(first);
            let arity = subs.len();
            let mut q2 = subs;
            q2.extend_from_slice(rest);
            useful(&specialize(matrix, &c, arity), &q2, sigs)
        }
        // q の先頭がワイルドカード。
        None => {
            let present = column_heads(matrix);
            match complete_signature(&present, sigs) {
                // 完全シグネチャなら各構築子で特殊化し、どれかで useful なら useful。
                Some(sig) => sig.into_iter().any(|(c, arity)| {
                    let mut q2 = vec![Pattern::Wildcard; arity];
                    q2.extend_from_slice(rest);
                    useful(&specialize(matrix, &c, arity), &q2, sigs)
                }),
                // 不完全（or 無限）ならデフォルト行列で進む。
                None => useful(&default_matrix(matrix), rest, sigs),
            }
        }
    }
}

// 反例生成 I(P, n): n 列の行列 P が覆い切れない値の例（witness）を返す。None なら網羅的。
// 網羅性検査の本体。witness はエラーメッセージで「足りないパターン」として見せる。
fn missing(matrix: &[Vec<Pattern>], n: usize, sigs: &Sigs) -> Option<Vec<Pattern>> {
    if n == 0 {
        return if matrix.is_empty() {
            Some(Vec::new())
        } else {
            None
        };
    }
    let present = column_heads(matrix);
    match complete_signature(&present, sigs) {
        // 完全: ある構築子の下に覆えない値があれば、それを頭に被せて witness を組む。
        Some(sig) => {
            for (c, arity) in sig {
                if let Some(w) = missing(&specialize(matrix, &c, arity), arity + n - 1, sigs) {
                    let (subs, rest) = w.split_at(arity);
                    let mut out = vec![head_to_pattern(&c, subs.to_vec())];
                    out.extend_from_slice(rest);
                    return Some(out);
                }
            }
            None
        }
        // 不完全: デフォルト行列が覆えない値があれば、頭に「足りない構築子」を被せる。
        None => missing(&default_matrix(matrix), n - 1, sigs).map(|w| {
            let mut out = vec![missing_head(&present, sigs)];
            out.extend_from_slice(&w);
            out
        }),
    }
}

// 不完全な列に対して「まだ現れていない頭」のパターンを 1 つ作る（witness の頭用）。
// 有限型なら欠けている構築子、Num や空列・不明なら _。
fn missing_head(present: &[Head], sigs: &Sigs) -> Pattern {
    match present.first() {
        Some(Head::Bool(_)) => {
            // 不完全なら true/false のどちらかが欠けている。
            let missing_val = !present.contains(&Head::Bool(true));
            Pattern::Bool(missing_val)
        }
        Some(Head::Ctor(name)) => {
            if let Some(sibs) = sigs.siblings(name) {
                for (n, a) in sibs {
                    if !present.contains(&Head::Ctor(n.clone())) {
                        return Pattern::Ctor(n.clone(), vec![Pattern::Wildcard; *a]);
                    }
                }
            }
            Pattern::Wildcard
        }
        _ => Pattern::Wildcard, // Int/String（無限）or 空列
    }
}

// パターンを人間向け文字列に描画する（エラーメッセージ用）。
fn render_pattern(p: &Pattern) -> String {
    match p {
        Pattern::Wildcard => "_".to_string(),
        Pattern::Var(name) => name.clone(),
        Pattern::Bool(b) => b.to_string(),
        Pattern::Int(n) => n.to_string(),
        Pattern::Str(s) => format!("{:?}", s),
        Pattern::Ctor(name, subs) => {
            if subs.is_empty() {
                name.clone()
            } else {
                let inner: Vec<String> = subs.iter().map(render_pattern).collect();
                format!("{}({})", name, inner.join(", "))
            }
        }
        Pattern::Or(alts) => alts
            .iter()
            .map(render_pattern)
            .collect::<Vec<_>>()
            .join(" | "),
    }
}

// 1 つの case 式の腕を検査する。or パターンは展開して行列の複数行にしてから、
// 到達不能を先に（具体的な腕を指せる）、次に網羅性を見る。
// 各行は 1 列（[パターン]）。展開後は or を含まないので usefulness は or を知らない。
// 1 つの case 式の腕を検査する。エラーは case 式全体の span に寄せる。
fn check_case(arms: &[(Pattern, Expr)], sigs: &Sigs, span: Span) -> Result<(), LocatedError> {
    // 到達不能: 腕 i の展開のどれかが「上の腕の展開全体」に対して useful なら到達可能。
    // どの展開も useful でなければ腕全体が死んでいる。
    let mut prior: Vec<Vec<Pattern>> = Vec::new();
    for (pattern, _) in arms {
        let rows: Vec<Vec<Pattern>> = expand(pattern).into_iter().map(|p| vec![p]).collect();
        let reachable = rows.iter().any(|row| useful(&prior, row, sigs));
        if !reachable {
            return Err(LocatedError::at(
                AwaError::UnreachablePattern(render_pattern(pattern)),
                span,
            ));
        }
        prior.extend(rows);
    }
    // 網羅性: 全腕の展開を集めた行列に、覆えない値（witness）が無いか。
    if let Some(witness) = missing(&prior, 1, sigs) {
        return Err(LocatedError::at(
            AwaError::NonExhaustiveMatch {
                witness: render_pattern(&witness[0]),
            },
            span,
        ));
    }
    Ok(())
}

// ===== プログラム全体の走査 =====
// 型検査が通った後に呼ぶ。全 case 式を見つけて検査する。型宣言はその場で sigs に
// 足す（ブロック内のローカル型はブロックスコープ限り）。sigs は呼び出し側（Checker）が
// トップレベル型を積んだものを渡す（REPL の行またぎ参照のため）。
pub fn check_matches(stmts: &[Stmt], sigs: &Sigs) -> Result<(), LocatedError> {
    let mut local = sigs.clone();
    check_stmts(stmts, &mut local)
}

fn check_stmts(stmts: &[Stmt], sigs: &mut Sigs) -> Result<(), LocatedError> {
    for stmt in stmts {
        match stmt {
            Stmt::Type(def) => sigs.add(def),
            Stmt::Let(_, _, e) | Stmt::Expr(e) => check_expr(e, sigs)?,
            // impl の各メソッド本体も case を含みうるので走査する。
            Stmt::Impl(def) => {
                for m in &def.methods {
                    check_expr(&m.body, sigs)?;
                }
            }
        }
    }
    Ok(())
}

fn check_expr(expr: &Expr, sigs: &Sigs) -> Result<(), LocatedError> {
    match &expr.kind {
        ExprKind::Int(_)
        | ExprKind::Long(_)
        | ExprKind::Float(_)
        | ExprKind::Str(_)
        | ExprKind::Bool(_)
        | ExprKind::Var(_) => Ok(()),
        ExprKind::Neg(x) => check_expr(x, sigs),
        ExprKind::BinOp(l, _, r) => {
            check_expr(l, sigs)?;
            check_expr(r, sigs)
        }
        ExprKind::If(c, t, e) => {
            check_expr(c, sigs)?;
            check_expr(t, sigs)?;
            check_expr(e, sigs)
        }
        ExprKind::Lambda(_, _, body) => check_expr(body, sigs),
        ExprKind::Call(callee, args) => {
            check_expr(callee, sigs)?;
            for a in args {
                check_expr(a, sigs)?;
            }
            Ok(())
        }
        ExprKind::Block(stmts) => {
            // ブロック内のローカル型はこのスコープ限り。
            let mut local = sigs.clone();
            check_stmts(stmts, &mut local)
        }
        ExprKind::Case(subject, arms) => {
            check_expr(subject, sigs)?;
            for (_, body) in arms {
                check_expr(body, sigs)?;
            }
            check_case(arms, sigs, expr.span)
        }
        ExprKind::MethodCall(receiver, _, args) => {
            check_expr(receiver, sigs)?;
            for a in args {
                check_expr(a, sigs)?;
            }
            Ok(())
        }
    }
}
