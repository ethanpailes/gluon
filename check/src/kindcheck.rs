use std::fmt;
use std::result::Result as StdResult;

use base::ast::{self, AstType};
use base::fnv::FnvMap;
use base::kind::{self, ArcKind, Kind, KindCache, KindEnv};
use base::merge;
use base::symbol::Symbol;
use base::types::{self, BuiltinType, Generic, Type, Walker};
use base::pos::{self, BytePos, HasSpan, Span, Spanned};

use substitution::{Constraints, Substitutable, Substitution};
use unify::{self, Error as UnifyError, Unifiable, Unifier, UnifierState};

pub type Error<I> = UnifyError<ArcKind, KindError<I>>;
pub type SpannedError<I> = Spanned<Error<I>, BytePos>;

pub type Result<T> = StdResult<T, SpannedError<Symbol>>;

/// Struct containing methods for kindchecking types
pub struct KindCheck<'a> {
    variables: Vec<Generic<Symbol>>,
    /// Type bindings local to the current kindcheck invocation
    locals: Vec<(Symbol, ArcKind)>,
    info: &'a (KindEnv + 'a),
    idents: &'a (ast::IdentEnv<Ident = Symbol> + 'a),
    pub subs: Substitution<ArcKind>,
    kind_cache: KindCache,
    /// A cached one argument kind function, `Type -> Type`
    function1_kind: ArcKind,
    /// A cached two argument kind function, `Type -> Type -> Type`
    function2_kind: ArcKind,
}

fn walk_move_kind<F>(kind: ArcKind, f: &mut F) -> ArcKind
where
    F: FnMut(&Kind) -> Option<ArcKind>,
{
    match walk_move_kind2(&kind, f) {
        Some(kind) => kind,
        None => kind,
    }
}
fn walk_move_kind2<F>(kind: &ArcKind, f: &mut F) -> Option<ArcKind>
where
    F: FnMut(&Kind) -> Option<ArcKind>,
{
    let new = f(kind);
    let new2 = {
        let kind = new.as_ref().unwrap_or(kind);
        match **kind {
            Kind::Function(ref arg, ref ret) => {
                let arg_new = walk_move_kind2(arg, f);
                let ret_new = walk_move_kind2(ret, f);
                merge::merge(arg, arg_new, ret, ret_new, Kind::function)
            }
            Kind::Hole | Kind::Type | Kind::Variable(_) | Kind::Row => None,
        }
    };
    new2.or(new)
}

impl<'a> KindCheck<'a> {
    pub fn new(
        info: &'a (KindEnv + 'a),
        idents: &'a (ast::IdentEnv<Ident = Symbol> + 'a),
        kind_cache: KindCache,
    ) -> KindCheck<'a> {
        let typ = kind_cache.typ();
        let function1_kind = Kind::function(typ.clone(), typ.clone());
        KindCheck {
            variables: Vec::new(),
            locals: Vec::new(),
            info: info,
            idents: idents,
            subs: Substitution::new(()),
            function1_kind: function1_kind.clone(),
            function2_kind: Kind::function(typ, function1_kind),
            kind_cache: kind_cache,
        }
    }

    pub fn add_local(&mut self, name: Symbol, kind: ArcKind) {
        self.locals.push((name, kind));
    }

    pub fn set_variables(&mut self, variables: &[Generic<Symbol>]) {
        self.variables.clear();
        self.variables.extend(variables.iter().cloned());
    }

    pub fn type_kind(&self) -> ArcKind {
        self.kind_cache.typ()
    }

    pub fn function1_kind(&self) -> ArcKind {
        self.function1_kind.clone()
    }

    pub fn function2_kind(&self) -> ArcKind {
        self.function2_kind.clone()
    }

    pub fn row_kind(&self) -> ArcKind {
        self.kind_cache.row()
    }

    pub fn instantiate_kinds(&mut self, kind: &mut ArcKind) {
        match *ArcKind::make_mut(kind) {
            // Can't assign a new var to `kind` here because it is borrowed mutably...
            // We'll rely on fall-through instead.
            Kind::Hole => (),
            Kind::Variable(_) => ice!("Unexpected kind variable while instantiating"),
            Kind::Function(ref mut lhs, ref mut rhs) => {
                self.instantiate_kinds(lhs);
                self.instantiate_kinds(rhs);
                return;
            }
            Kind::Row | Kind::Type => return,
        }
        *kind = self.subs.new_var();
    }

    fn find(&mut self, span: Span<BytePos>, id: &Symbol) -> Result<ArcKind> {
        let kind = self.variables
            .iter()
            .find(|var| var.id == *id)
            .map(|t| t.kind.clone())
            .or_else(|| self.locals.iter().find(|t| t.0 == *id).map(|t| t.1.clone()))
            .or_else(|| self.info.find_kind(id))
            .map_or_else(
                || {
                    let id_str = self.idents.string(id);
                    if id_str.starts_with(char::is_uppercase) {
                        Err(UnifyError::Other(KindError::UndefinedType(id.clone())))
                    } else {
                        // Create a new variable
                        self.locals.push((id.clone(), self.subs.new_var()));
                        Ok(self.locals.last().unwrap().1.clone())
                    }
                },
                Ok,
            );

        if let Ok(ref kind) = kind {
            debug!("Find kind: {} => {}", self.idents.string(&id), kind);
        }

        kind.map_err(|err| pos::spanned(span, err))
    }

    // Kindhecks `typ`, infering it to be of kind `Type`
    pub fn kindcheck_type(&mut self, typ: &mut AstType<Symbol>) -> Result<ArcKind> {
        let type_kind = self.type_kind();
        self.kindcheck_expected(typ, &type_kind)
    }

    pub fn kindcheck_expected(
        &mut self,
        typ: &mut AstType<Symbol>,
        expected: &ArcKind,
    ) -> Result<ArcKind> {
        let kind = self.kindcheck(typ)?;
        let kind = self.unify(typ.span(), expected, kind)?;
        self.finalize_type(typ);
        Ok(kind)
    }

    fn builtin_kind(&self, typ: BuiltinType) -> ArcKind {
        match typ {
            BuiltinType::String
            | BuiltinType::Byte
            | BuiltinType::Char
            | BuiltinType::Int
            | BuiltinType::Float => self.type_kind(),
            BuiltinType::Array => self.function1_kind(),
            BuiltinType::Function => self.function2_kind(),
        }
    }

    fn kindcheck(&mut self, typ: &mut AstType<Symbol>) -> Result<ArcKind> {
        let span = typ.span();
        match **typ {
            Type::Hole | Type::Opaque | Type::Variable(_) => Ok(self.subs.new_var()),
            Type::Skolem(ref mut skolem) => {
                skolem.kind = self.find(span, &skolem.name)?;
                Ok(skolem.kind.clone())
            }
            Type::Generic(ref mut gen) => {
                gen.kind = self.find(span, &gen.id)?;
                Ok(gen.kind.clone())
            }
            Type::Builtin(builtin_typ) => Ok(self.builtin_kind(builtin_typ)),
            Type::Forall(ref mut params, ref mut typ, _) => {
                for param in &mut *params {
                    param.kind = self.subs.new_var();
                    self.locals.push((param.id.clone(), param.kind.clone()));
                }
                let ret_kind = self.kindcheck(typ)?;

                let offset = self.locals.len() - params.len();
                self.locals.drain(offset..);

                Ok(ret_kind)
            }
            Type::App(ref mut ctor, ref mut args) => {
                let mut kind = self.kindcheck(ctor)?;
                for arg in args {
                    let f = Kind::function(self.subs.new_var(), self.subs.new_var());
                    kind = self.unify(arg.span(), &f, kind)?;
                    kind = match *kind {
                        Kind::Function(ref arg_kind, ref ret) => {
                            let actual = self.kindcheck(arg)?;
                            self.unify(arg.span(), arg_kind, actual)?;
                            ret.clone()
                        }
                        _ => {
                            return Err(pos::spanned(
                                arg.span(),
                                UnifyError::TypeMismatch(self.function1_kind(), kind.clone()),
                            ))
                        }
                    };
                }
                Ok(kind)
            }
            Type::Variant(ref mut row) => {
                for field in types::row_iter_mut(row) {
                    let kind = self.kindcheck(&mut field.typ)?;
                    let type_kind = self.type_kind();
                    self.unify(field.typ.span(), &type_kind, kind)?;
                }

                Ok(self.type_kind())
            }
            Type::Record(ref mut row) => {
                let kind = self.kindcheck(row)?;
                let row_kind = self.row_kind();
                self.unify(span, &row_kind, kind)?;
                Ok(self.type_kind())
            }
            Type::ExtendRow {
                types: _,
                ref mut fields,
                ref mut rest,
            } => {
                for field in fields {
                    let kind = self.kindcheck(&mut field.typ)?;
                    let type_kind = self.type_kind();
                    self.unify(field.typ.span(), &type_kind, kind)?;
                }

                let kind = self.kindcheck(rest)?;
                let row_kind = self.row_kind();
                self.unify(rest.span(), &row_kind, kind)?;

                Ok(row_kind)
            }
            Type::EmptyRow => Ok(self.row_kind()),
            Type::Ident(ref id) => self.find(span, id),
            Type::Alias(ref alias) => self.find(span, &alias.name),
        }
    }

    fn unify(
        &mut self,
        span: Span<BytePos>,
        expected: &ArcKind,
        mut actual: ArcKind,
    ) -> Result<ArcKind> {
        debug!("Unify {:?} <=> {:?}", expected, actual);
        let result = unify::unify(&self.subs, (), expected, &actual);
        match result {
            Ok(k) => Ok(k),
            Err(_errors) => {
                let mut expected = expected.clone();
                expected = update_kind(&self.subs, expected, None);
                actual = update_kind(&self.subs, actual, None);
                Err(pos::spanned(
                    span,
                    UnifyError::TypeMismatch(expected, actual),
                ))
            }
        }
    }

    pub fn finalize_type(&self, typ: &mut AstType<Symbol>) {
        let default = Some(&self.kind_cache.typ);
        types::walk_type_mut(typ, &mut |typ: &mut AstType<Symbol>| match **typ {
            Type::Variable(ref mut var) => {
                var.kind = update_kind(&self.subs, var.kind.clone(), default);
            }
            Type::Generic(ref mut var) => *var = self.finalize_generic(var),
            Type::Forall(ref mut params, _, _) => for param in params {
                *param = self.finalize_generic(&param);
            },
            _ => (),
        });
    }
    pub fn finalize_generic(&self, var: &Generic<Symbol>) -> Generic<Symbol> {
        let mut kind = var.kind.clone();
        kind = update_kind(&self.subs, kind, Some(&self.kind_cache.typ));
        Generic::new(var.id.clone(), kind)
    }
}

fn update_kind(subs: &Substitution<ArcKind>, kind: ArcKind, default: Option<&ArcKind>) -> ArcKind {
    walk_move_kind(kind, &mut |kind| match *kind {
        Kind::Variable(id) => subs.find_type_for_var(id)
            .map(|kind| update_kind(subs, kind.clone(), default))
            .or_else(|| default.cloned()),
        _ => None,
    })
}

/// Enumeration possible errors other than mismatch and occurs when kindchecking
#[derive(Debug, PartialEq)]
pub enum KindError<I> {
    /// The type is not defined in the current scope
    UndefinedType(I),
}

impl<I> fmt::Display for KindError<I>
where
    I: fmt::Display,
{
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match *self {
            KindError::UndefinedType(ref name) => write!(f, "Type '{}' is not defined", name),
        }
    }
}

pub fn fmt_kind_error<I>(error: &Error<I>, f: &mut fmt::Formatter) -> fmt::Result
where
    I: fmt::Display,
{
    use unify::Error::*;
    match *error {
        TypeMismatch(ref expected, ref actual) => write!(
            f,
            "Kind mismatch\nExpected: {}\nFound: {}",
            expected, actual
        ),
        Substitution(ref err) => write!(f, "{}", err),
        Other(ref err) => write!(f, "{}", err),
    }
}

impl Substitutable for ArcKind {
    type Variable = u32;
    type Factory = ();

    fn from_variable(x: u32) -> ArcKind {
        Kind::variable(x)
    }

    fn get_var(&self) -> Option<&u32> {
        match **self {
            Kind::Variable(ref var) => Some(var),
            _ => None,
        }
    }

    fn traverse<'a, F>(&'a self, f: &mut F)
    where
        F: Walker<'a, ArcKind>,
    {
        kind::walk_kind(self, f);
    }
    fn instantiate(
        &self,
        _subs: &Substitution<Self>,
        _constraints: &FnvMap<Symbol, Constraints<Self>>,
    ) -> Self {
        self.clone()
    }
}

impl<S> Unifiable<S> for ArcKind {
    type Error = KindError<Symbol>;

    fn zip_match<U>(
        &self,
        other: &Self,
        unifier: &mut UnifierState<S, U>,
    ) -> StdResult<Option<Self>, Error<Symbol>>
    where
        UnifierState<S, U>: Unifier<S, Self>,
    {
        match (&**self, &**other) {
            (&Kind::Function(ref l1, ref l2), &Kind::Function(ref r1, ref r2)) => {
                let a = unifier.try_match(l1, r1);
                let r = unifier.try_match(l2, r2);
                Ok(merge::merge(l1, a, l2, r, Kind::function))
            }
            (l, r) => if l == r {
                Ok(None)
            } else {
                Err(UnifyError::TypeMismatch(self.clone(), other.clone()))
            },
        }
    }
}
