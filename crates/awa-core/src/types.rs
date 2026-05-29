use std::collections::{HashMap, HashSet};
use std::fmt;

use crate::error::AwaError;

// ===== 型の表現 =====
// Hindley-Milner 型推論で扱う型。具体型（Num/Bool/Unit）に加え、
// 関数型と「型変数」を持つ。型変数は「まだ分からない型」を表すプレースホルダで、
// 推論が進むにつれ unify によって具体型へ埋まっていく。
#[derive(Debug, Clone, PartialEq)]
pub enum Type {
    Int,
    Long,
    Float,
    Str,
    Bool,
    Unit,
    // 関数型。多引数なので引数型のリストと戻り値型を持つ。例: (Num, Num) -> Bool
    Fun(Vec<Type>, Box<Type>),
    // 型変数。一意な ID で識別する（a, b, ... に相当）。
    Var(u32),
    // 型構築子の適用（ADT）。name と型引数のリスト。例: Option(Num), List(a), Result(a, e)。
    // 名前で同一性を見る nominal な型。引数ゼロでも Con（例: ユーザー定義の Color）。
    Con(String, Vec<Type>),
}

impl fmt::Display for Type {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            Type::Int => write!(f, "Int"),
            Type::Long => write!(f, "Long"),
            Type::Float => write!(f, "Float"),
            Type::Str => write!(f, "String"),
            Type::Bool => write!(f, "Bool"),
            Type::Unit => write!(f, "Nil"),
            Type::Fun(params, ret) => {
                write!(f, "fn(")?;
                for (i, p) in params.iter().enumerate() {
                    if i > 0 {
                        write!(f, ", ")?;
                    }
                    write!(f, "{}", p)?;
                }
                write!(f, ") -> {}", ret)
            }
            // 型変数は ID をアルファベットに変換して表示（0→a, 1→b, ...）。
            Type::Var(id) => {
                let letter = (b'a' + (*id % 26) as u8) as char;
                let suffix = id / 26;
                if suffix == 0 {
                    write!(f, "{}", letter)
                } else {
                    write!(f, "{}{}", letter, suffix)
                }
            }
            // タプルは (A, B) と表示する（Tuple(A, B) ではなく）。
            Type::Con(name, args) if name == "Tuple" => {
                write!(f, "(")?;
                for (i, a) in args.iter().enumerate() {
                    if i > 0 {
                        write!(f, ", ")?;
                    }
                    write!(f, "{}", a)?;
                }
                write!(f, ")")
            }
            Type::Con(name, args) => {
                write!(f, "{}", name)?;
                if !args.is_empty() {
                    write!(f, "(")?;
                    for (i, a) in args.iter().enumerate() {
                        if i > 0 {
                            write!(f, ", ")?;
                        }
                        write!(f, "{}", a)?;
                    }
                    write!(f, ")")?;
                }
                Ok(())
            }
        }
    }
}

// 型を人間向けに表示する文字列にする。Display と違い、型変数を「出現順に a, b, c...」
// へ振り直して読みやすくする（内部 ID は飛び飛びなので表示用に正規化）。
// 多相な型もそのまま fn(a) -> a のように出す（Gleam 風に ∀ は付けない）。
pub fn render_type(ty: &Type) -> String {
    fn go(ty: &Type, names: &mut HashMap<u32, char>, next: &mut u8, out: &mut String) {
        match ty {
            Type::Int => out.push_str("Int"),
            Type::Long => out.push_str("Long"),
            Type::Float => out.push_str("Float"),
            Type::Str => out.push_str("String"),
            Type::Bool => out.push_str("Bool"),
            Type::Unit => out.push_str("Nil"),
            Type::Fun(params, ret) => {
                out.push_str("fn(");
                for (i, p) in params.iter().enumerate() {
                    if i > 0 {
                        out.push_str(", ");
                    }
                    go(p, names, next, out);
                }
                out.push_str(") -> ");
                go(ret, names, next, out);
            }
            Type::Var(id) => {
                let c = *names.entry(*id).or_insert_with(|| {
                    let c = (b'a' + *next % 26) as char;
                    *next += 1;
                    c
                });
                out.push(c);
            }
            // タプルは (A, B) と正規化表示する。
            Type::Con(name, args) if name == "Tuple" => {
                out.push('(');
                for (i, a) in args.iter().enumerate() {
                    if i > 0 {
                        out.push_str(", ");
                    }
                    go(a, names, next, out);
                }
                out.push(')');
            }
            Type::Con(name, args) => {
                out.push_str(name);
                if !args.is_empty() {
                    out.push('(');
                    for (i, a) in args.iter().enumerate() {
                        if i > 0 {
                            out.push_str(", ");
                        }
                        go(a, names, next, out);
                    }
                    out.push(')');
                }
            }
        }
    }
    let mut out = String::new();
    go(ty, &mut HashMap::new(), &mut 0, &mut out);
    out
}

// ===== 置換（Substitution）=====
// 「型変数 ID → 確定した型」の対応表。推論が進むと埋まっていく。
// これが型推論の状態の本体。
pub type Subst = HashMap<u32, Type>;

// 型変数を採番するカウンタ。fresh() を呼ぶたびに新しい型変数を作る。
#[derive(Debug, Default)]
pub struct VarGen {
    next: u32,
}

impl VarGen {
    // 新しい型変数を 1 つ作る。「まだ分からない型」が必要なときに呼ぶ。
    pub fn fresh(&mut self) -> Type {
        let id = self.next;
        self.next += 1;
        Type::Var(id)
    }
}

// 置換を型に適用し、埋まっている型変数を実体へ置き換える。
// 例: subst = {0 -> Num} のとき apply(Fun([Var(0)], Var(0))) = Fun([Num], Num)。
// 型変数のチェーン（Var(0) -> Var(1) -> Num）も再帰的に辿って解決する。
pub fn apply(subst: &Subst, ty: &Type) -> Type {
    match ty {
        Type::Int | Type::Long | Type::Float | Type::Str | Type::Bool | Type::Unit => ty.clone(),
        Type::Fun(params, ret) => {
            let params = params.iter().map(|p| apply(subst, p)).collect();
            let ret = Box::new(apply(subst, ret));
            Type::Fun(params, ret)
        }
        Type::Con(name, args) => {
            let args = args.iter().map(|a| apply(subst, a)).collect();
            Type::Con(name.clone(), args)
        }
        Type::Var(id) => match subst.get(id) {
            // 埋まっていれば、その先も再帰的に解決する（チェーンを辿る）。
            Some(bound) => apply(subst, bound),
            // 埋まっていなければ型変数のまま。
            None => ty.clone(),
        },
    }
}

// ===== 型スキーム（多相）=====
// ∀(vars). ty。多相な束縛を表す。vars は「任意の型でよい」と量化された型変数 ID。
// 例: 恒等関数 id は ∀a. fn(a) -> a ＝ Scheme { vars: [a], ty: fn(a) -> a }。
// 単相な型（量化なし）は vars が空のスキーム（Scheme::mono）。
#[derive(Debug, Clone, PartialEq)]
pub struct Scheme {
    pub vars: Vec<u32>,
    pub ty: Type,
}

impl Scheme {
    // 単相スキーム（量化なし）。ラムダ引数や再帰の placeholder に使う。
    pub fn mono(ty: Type) -> Self {
        Scheme {
            vars: Vec::new(),
            ty,
        }
    }
}

// 型に現れる自由型変数の集合。subst で解決した後の姿で数える
// （すでに確定した型変数は「自由」ではないので apply 後に見る）。
pub fn ftv(subst: &Subst, ty: &Type) -> HashSet<u32> {
    match apply(subst, ty) {
        Type::Var(id) => HashSet::from([id]),
        Type::Fun(params, ret) => {
            let mut s = HashSet::new();
            for p in &params {
                s.extend(ftv(subst, p));
            }
            s.extend(ftv(subst, &ret));
            s
        }
        Type::Con(_, args) => {
            let mut s = HashSet::new();
            for a in &args {
                s.extend(ftv(subst, a));
            }
            s
        }
        Type::Int | Type::Long | Type::Float | Type::Str | Type::Bool | Type::Unit => HashSet::new(),
    }
}

// 具体化（instantiate）: スキームの量化変数を fresh な型変数に差し替えて型を取り出す。
// 変数を「参照」するたびに呼ぶ。これにより同じ多相関数の各使用箇所が独立した
// 型変数を得て、id(1) と id(true) が互いに干渉しなくなる。
// 量化されていない型変数（環境に縛られた変数）はそのまま残す。
pub fn instantiate(scheme: &Scheme, vargen: &mut VarGen) -> Type {
    let mapping: Subst = scheme.vars.iter().map(|&id| (id, vargen.fresh())).collect();
    apply(&mapping, &scheme.ty)
}

// occurs check: 型変数 id が型 ty の中に現れるか。
// 現れる状態で id を ty に束縛すると無限型（a = a -> b 等）になるので、それを防ぐ。
fn occurs(subst: &Subst, id: u32, ty: &Type) -> bool {
    match apply(subst, ty) {
        Type::Var(other) => other == id,
        Type::Fun(params, ret) => {
            params.iter().any(|p| occurs(subst, id, p)) || occurs(subst, id, &ret)
        }
        Type::Con(_, args) => args.iter().any(|a| occurs(subst, id, a)),
        Type::Int | Type::Long | Type::Float | Type::Str | Type::Bool | Type::Unit => false,
    }
}

// 型変数 id を型 ty に束縛する（subst を更新）。occurs check 付き。
fn bind(subst: &mut Subst, id: u32, ty: &Type) -> Result<(), AwaError> {
    // 自分自身への束縛（a = a）は何もしなくてよい。
    if let Type::Var(other) = ty {
        if *other == id {
            return Ok(());
        }
    }
    if occurs(subst, id, ty) {
        return Err(AwaError::InfiniteType);
    }
    subst.insert(id, ty.clone());
    Ok(())
}

// ===== 単一化（unify）=====
// 2 つの型を「同じであるべき」と突き合わせ、必要なら型変数を束縛して subst を更新する。
// 型推論の心臓部。
//
// TODO(あなた): unify の本体を実装してください。
//
// まず両辺に現在の置換を適用して最新の姿にする（apply 済みの a, b で考える）。
// 場合分け:
//   1. 具体型同士で一致（Num と Num、Bool と Bool、Unit と Unit）→ Ok(())。何もしない。
//   2. 片方が型変数 Var(id) → bind(subst, id, &相手) を呼ぶ。
//      （両方が型変数のときもこれで OK。occurs/自己束縛は bind が面倒を見る）
//   3. 両方が関数型 Fun(ps1, r1) と Fun(ps2, r2):
//      - 引数の個数が違えば Err(AwaError::ArityMismatch { expected, found })
//      - 各引数同士を unify（ps1[i] と ps2[i]）。subst は順に更新されるので、
//        ループの中で再帰呼び出しする。
//      - 最後に戻り値同士 r1 と r2 を unify。
//   4. それ以外（Num と Bool など噛み合わない）→ Err(AwaError::CannotUnify {...})。
//
// ヒント:
//   - 先頭で `let a = apply(subst, t1); let b = apply(subst, t2);` として
//     以降は a, b を使うと、すでに埋まった型変数に惑わされない。
//   - match (a, b) { ... } で上の場合分けを書ける。
//   - 関数型の引数ループ後、戻り値の unify は unify(subst, &r1, &r2) を再帰呼び出し。
pub fn unify(subst: &mut Subst, t1: &Type, t2: &Type) -> Result<(), AwaError> {
    let a = apply(subst, t1);
    let b = apply(subst, t2);
    match (a, b) {
        (Type::Int, Type::Int)
        | (Type::Long, Type::Long)
        | (Type::Float, Type::Float)
        | (Type::Str, Type::Str)
        | (Type::Bool, Type::Bool)
        | (Type::Unit, Type::Unit) => Ok(()),
        (Type::Var(id), ty) | (ty, Type::Var(id)) => bind(subst, id, &ty),
        (Type::Fun(ps1, r1), Type::Fun(ps2, r2)) => {
            if ps1.len() != ps2.len() {
                return Err(AwaError::ArityMismatch {
                    expected: ps1.len(),
                    found: ps2.len(),
                });
            }
            // 各引数同士を単一化
            for (p1, p2) in ps1.iter().zip(ps2.iter()) {
                unify(subst, p1, p2)?;
            }
            // 戻り値同士を単一化
            unify(subst, &r1, &r2)
        }
        // 型構築子同士: 名前が同じで引数の個数が一致するなら、引数をペアごとに単一化。
        // 例: Option(a) と Option(Num) → a = Num。List(a) と Option(a) → 名前違いでエラー。
        (Type::Con(n1, a1), Type::Con(n2, a2)) => {
            if n1 != n2 || a1.len() != a2.len() {
                return Err(AwaError::CannotUnify {
                    left: Type::Con(n1, a1).to_string(),
                    right: Type::Con(n2, a2).to_string(),
                });
            }
            for (x, y) in a1.iter().zip(a2.iter()) {
                unify(subst, x, y)?;
            }
            Ok(())
        }
        // それ以外は単一化不可能
        (a, b) => Err(AwaError::CannotUnify {
            left: a.to_string(),
            right: b.to_string(),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 具体型同士の単一化() {
        let mut s = Subst::new();
        assert_eq!(unify(&mut s, &Type::Int, &Type::Int), Ok(()));
        assert_eq!(unify(&mut s, &Type::Bool, &Type::Bool), Ok(()));
    }

    #[test]
    fn 噛み合わない具体型はエラー() {
        let mut s = Subst::new();
        assert!(unify(&mut s, &Type::Int, &Type::Bool).is_err());
    }

    #[test]
    fn 型変数は相手の型に束縛される() {
        let mut s = Subst::new();
        // a と Num を unify すると a -> Num が記録される。
        unify(&mut s, &Type::Var(0), &Type::Int).unwrap();
        assert_eq!(apply(&s, &Type::Var(0)), Type::Int);
    }

    #[test]
    fn 型変数同士の単一化を経由して解決() {
        let mut s = Subst::new();
        // a = b, b = Bool ⇒ a も Bool に解決される。
        unify(&mut s, &Type::Var(0), &Type::Var(1)).unwrap();
        unify(&mut s, &Type::Var(1), &Type::Bool).unwrap();
        assert_eq!(apply(&s, &Type::Var(0)), Type::Bool);
    }

    #[test]
    fn 関数型の単一化() {
        let mut s = Subst::new();
        // fn(a) -> Num  と  fn(Bool) -> b  を unify ⇒ a=Bool, b=Num
        let f1 = Type::Fun(vec![Type::Var(0)], Box::new(Type::Int));
        let f2 = Type::Fun(vec![Type::Bool], Box::new(Type::Var(1)));
        unify(&mut s, &f1, &f2).unwrap();
        assert_eq!(apply(&s, &Type::Var(0)), Type::Bool);
        assert_eq!(apply(&s, &Type::Var(1)), Type::Int);
    }

    #[test]
    fn 引数の個数が違う関数型はエラー() {
        let mut s = Subst::new();
        let f1 = Type::Fun(vec![Type::Int], Box::new(Type::Int));
        let f2 = Type::Fun(vec![Type::Int, Type::Int], Box::new(Type::Int));
        assert!(unify(&mut s, &f1, &f2).is_err());
    }

    #[test]
    fn occurs_check_無限型を弾く() {
        let mut s = Subst::new();
        // a = fn(a) -> b は無限型なのでエラー。
        let f = Type::Fun(vec![Type::Var(0)], Box::new(Type::Var(1)));
        assert_eq!(
            unify(&mut s, &Type::Var(0), &f),
            Err(AwaError::InfiniteType)
        );
    }

    #[test]
    fn fresh_は異なる型変数を作る() {
        let mut g = VarGen::default();
        let a = g.fresh();
        let b = g.fresh();
        assert_ne!(a, b);
    }
}
