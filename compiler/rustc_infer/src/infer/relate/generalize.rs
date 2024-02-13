use std::mem;

use rustc_data_structures::sso::SsoHashMap;
use rustc_data_structures::stack::ensure_sufficient_stack;
use rustc_hir::def_id::DefId;
use rustc_middle::infer::unify_key::ConstVariableValue;
use rustc_middle::ty::error::TypeError;
use rustc_middle::ty::relate::{self, Relate, RelateResult, TypeRelation};
use rustc_middle::ty::visit::MaxUniverse;
use rustc_middle::ty::{self, InferConst, Term, Ty, TyCtxt, TypeVisitable, TypeVisitableExt};
use rustc_span::Span;

use crate::infer::nll_relate::TypeRelatingDelegate;
use crate::infer::type_variable::{TypeVariableOrigin, TypeVariableOriginKind, TypeVariableValue};
use crate::infer::{InferCtxt, RegionVariableOrigin};

/// Attempts to generalize `term` for the type variable `for_vid`.
/// This checks for cycles -- that is, whether the type `term`
/// references `for_vid`.
pub fn generalize<'tcx, D: GeneralizerDelegate<'tcx>, T: Into<Term<'tcx>> + Relate<'tcx>>(
    infcx: &InferCtxt<'tcx>,
    delegate: &mut D,
    term: T,
    for_vid: impl Into<ty::TermVid>,
    ambient_variance: ty::Variance,
) -> RelateResult<'tcx, Generalization<T>> {
    let (for_universe, root_vid) = match for_vid.into() {
        ty::TermVid::Ty(ty_vid) => (
            infcx.probe_ty_var(ty_vid).unwrap_err(),
            ty::TermVid::Ty(infcx.inner.borrow_mut().type_variables().sub_root_var(ty_vid)),
        ),
        ty::TermVid::Const(ct_vid) => (
            infcx.probe_const_var(ct_vid).unwrap_err(),
            ty::TermVid::Const(infcx.inner.borrow_mut().const_unification_table().find(ct_vid).vid),
        ),
    };

    let mut generalizer = Generalizer {
        infcx,
        delegate,
        ambient_variance,
        root_vid,
        for_universe,
        root_term: term.into(),
        in_alias: false,
        needs_wf: false,
        cache: Default::default(),
    };

    assert!(!term.has_escaping_bound_vars());
    let value_may_be_infer = generalizer.relate(term, term)?;
    let needs_wf = generalizer.needs_wf;
    Ok(Generalization { value_may_be_infer, needs_wf })
}

/// Abstracts the handling of region vars between HIR and MIR/NLL typechecking
/// in the generalizer code.
pub trait GeneralizerDelegate<'tcx> {
    fn forbid_inference_vars() -> bool;

    fn span(&self) -> Span;

    fn generalize_region(&mut self, universe: ty::UniverseIndex) -> ty::Region<'tcx>;
}

pub struct CombineDelegate<'cx, 'tcx> {
    pub infcx: &'cx InferCtxt<'tcx>,
    pub span: Span,
}

impl<'tcx> GeneralizerDelegate<'tcx> for CombineDelegate<'_, 'tcx> {
    fn forbid_inference_vars() -> bool {
        false
    }

    fn span(&self) -> Span {
        self.span
    }

    fn generalize_region(&mut self, universe: ty::UniverseIndex) -> ty::Region<'tcx> {
        // FIXME: This is non-ideal because we don't give a
        // very descriptive origin for this region variable.
        self.infcx
            .next_region_var_in_universe(RegionVariableOrigin::MiscVariable(self.span), universe)
    }
}

impl<'tcx, T> GeneralizerDelegate<'tcx> for T
where
    T: TypeRelatingDelegate<'tcx>,
{
    fn forbid_inference_vars() -> bool {
        <Self as TypeRelatingDelegate<'tcx>>::forbid_inference_vars()
    }

    fn span(&self) -> Span {
        <Self as TypeRelatingDelegate<'tcx>>::span(&self)
    }

    fn generalize_region(&mut self, universe: ty::UniverseIndex) -> ty::Region<'tcx> {
        <Self as TypeRelatingDelegate<'tcx>>::generalize_existential(self, universe)
    }
}

/// The "generalizer" is used when handling inference variables.
///
/// The basic strategy for handling a constraint like `?A <: B` is to
/// apply a "generalization strategy" to the term `B` -- this replaces
/// all the lifetimes in the term `B` with fresh inference variables.
/// (You can read more about the strategy in this [blog post].)
///
/// As an example, if we had `?A <: &'x u32`, we would generalize `&'x
/// u32` to `&'0 u32` where `'0` is a fresh variable. This becomes the
/// value of `A`. Finally, we relate `&'0 u32 <: &'x u32`, which
/// establishes `'0: 'x` as a constraint.
///
/// [blog post]: https://is.gd/0hKvIr
struct Generalizer<'me, 'tcx, D> {
    infcx: &'me InferCtxt<'tcx>,

    /// This is used to abstract the behaviors of the three previous
    /// generalizer-like implementations (`Generalizer`, `TypeGeneralizer`,
    /// and `ConstInferUnifier`). See [`GeneralizerDelegate`] for more
    /// information.
    delegate: &'me mut D,

    /// After we generalize this type, we are going to relate it to
    /// some other type. What will be the variance at this point?
    ambient_variance: ty::Variance,

    /// The vid of the type variable that is in the process of being
    /// instantiated. If we find this within the value we are folding,
    /// that means we would have created a cyclic value.
    root_vid: ty::TermVid,

    /// The universe of the type variable that is in the process of being
    /// instantiated. If we find anything that this universe cannot name,
    /// we reject the relation.
    for_universe: ty::UniverseIndex,

    /// The root term (const or type) we're generalizing. Used for cycle errors.
    root_term: Term<'tcx>,

    cache: SsoHashMap<Ty<'tcx>, Ty<'tcx>>,

    /// This is set once we're generalizing the arguments of an alias.
    ///
    /// This is necessary to correctly handle
    /// `<T as Bar<<?0 as Foo>::Assoc>::Assoc == ?0`. This equality can
    /// hold by either normalizing the outer or the inner associated type.
    in_alias: bool,

    /// See the field `needs_wf` in `Generalization`.
    needs_wf: bool,
}

impl<'tcx, D> Generalizer<'_, 'tcx, D> {
    /// Create an error that corresponds to the term kind in `root_term`
    fn cyclic_term_error(&self) -> TypeError<'tcx> {
        match self.root_term.unpack() {
            ty::TermKind::Ty(ty) => TypeError::CyclicTy(ty),
            ty::TermKind::Const(ct) => TypeError::CyclicConst(ct),
        }
    }
}

impl<'tcx, D> TypeRelation<'tcx> for Generalizer<'_, 'tcx, D>
where
    D: GeneralizerDelegate<'tcx>,
{
    fn tcx(&self) -> TyCtxt<'tcx> {
        self.infcx.tcx
    }

    fn tag(&self) -> &'static str {
        "Generalizer"
    }

    fn a_is_expected(&self) -> bool {
        true
    }

    fn relate_item_args(
        &mut self,
        item_def_id: DefId,
        a_arg: ty::GenericArgsRef<'tcx>,
        b_arg: ty::GenericArgsRef<'tcx>,
    ) -> RelateResult<'tcx, ty::GenericArgsRef<'tcx>> {
        if self.ambient_variance == ty::Variance::Invariant {
            // Avoid fetching the variance if we are in an invariant
            // context; no need, and it can induce dependency cycles
            // (e.g., #41849).
            relate::relate_args_invariantly(self, a_arg, b_arg)
        } else {
            let tcx = self.tcx();
            let opt_variances = tcx.variances_of(item_def_id);
            relate::relate_args_with_variances(
                self,
                item_def_id,
                opt_variances,
                a_arg,
                b_arg,
                false,
            )
        }
    }

    #[instrument(level = "debug", skip(self, variance, b), ret)]
    fn relate_with_variance<T: Relate<'tcx>>(
        &mut self,
        variance: ty::Variance,
        _info: ty::VarianceDiagInfo<'tcx>,
        a: T,
        b: T,
    ) -> RelateResult<'tcx, T> {
        let old_ambient_variance = self.ambient_variance;
        self.ambient_variance = self.ambient_variance.xform(variance);
        debug!(?self.ambient_variance, "new ambient variance");
        // Recursive calls to `relate` can overflow the stack. For example a deeper version of
        // `ui/associated-consts/issue-93775.rs`.
        let r = ensure_sufficient_stack(|| self.relate(a, b))?;
        self.ambient_variance = old_ambient_variance;
        Ok(r)
    }

    #[instrument(level = "debug", skip(self, t2), ret)]
    fn tys(&mut self, t: Ty<'tcx>, t2: Ty<'tcx>) -> RelateResult<'tcx, Ty<'tcx>> {
        assert_eq!(t, t2); // we are misusing TypeRelation here; both LHS and RHS ought to be ==

        if let Some(&result) = self.cache.get(&t) {
            return Ok(result);
        }

        // Check to see whether the type we are generalizing references
        // any other type variable related to `vid` via
        // subtyping. This is basically our "occurs check", preventing
        // us from creating infinitely sized types.
        let g = match *t.kind() {
            ty::Infer(ty::TyVar(_)) | ty::Infer(ty::IntVar(_)) | ty::Infer(ty::FloatVar(_))
                if D::forbid_inference_vars() =>
            {
                bug!("unexpected inference variable encountered in NLL generalization: {t}");
            }

            ty::Infer(ty::FreshTy(_) | ty::FreshIntTy(_) | ty::FreshFloatTy(_)) => {
                bug!("unexpected infer type: {t}")
            }

            ty::Infer(ty::TyVar(vid)) => {
                let mut inner = self.infcx.inner.borrow_mut();
                let vid = inner.type_variables().root_var(vid);
                let sub_vid = inner.type_variables().sub_root_var(vid);

                if ty::TermVid::Ty(sub_vid) == self.root_vid {
                    // If sub-roots are equal, then `root_vid` and
                    // `vid` are related via subtyping.
                    Err(self.cyclic_term_error())
                } else {
                    let probe = inner.type_variables().probe(vid);
                    match probe {
                        TypeVariableValue::Known { value: u } => {
                            drop(inner);
                            self.relate(u, u)
                        }
                        TypeVariableValue::Unknown { universe } => {
                            match self.ambient_variance {
                                // Invariant: no need to make a fresh type variable
                                // if we can name the universe.
                                ty::Invariant => {
                                    if self.for_universe.can_name(universe) {
                                        return Ok(t);
                                    }
                                }

                                // Bivariant: make a fresh var, but we
                                // may need a WF predicate. See
                                // comment on `needs_wf` field for
                                // more info.
                                ty::Bivariant => self.needs_wf = true,

                                // Co/contravariant: this will be
                                // sufficiently constrained later on.
                                ty::Covariant | ty::Contravariant => (),
                            }

                            let origin = inner.type_variables().var_origin(vid);
                            let new_var_id =
                                inner.type_variables().new_var(self.for_universe, origin);
                            let u = Ty::new_var(self.tcx(), new_var_id);

                            // Record that we replaced `vid` with `new_var_id` as part of a generalization
                            // operation. This is needed to detect cyclic types. To see why, see the
                            // docs in the `type_variables` module.
                            inner.type_variables().sub(vid, new_var_id);
                            debug!("replacing original vid={:?} with new={:?}", vid, u);
                            Ok(u)
                        }
                    }
                }
            }

            ty::Infer(ty::IntVar(_) | ty::FloatVar(_)) => {
                // No matter what mode we are in,
                // integer/floating-point types must be equal to be
                // relatable.
                Ok(t)
            }

            ty::Placeholder(placeholder) => {
                if self.for_universe.can_name(placeholder.universe) {
                    Ok(t)
                } else {
                    debug!(
                        "root universe {:?} cannot name placeholder in universe {:?}",
                        self.for_universe, placeholder.universe
                    );
                    Err(TypeError::Mismatch)
                }
            }

            ty::Alias(kind, data) => {
                // An occurs check failure inside of an alias does not mean
                // that the types definitely don't unify. We may be able
                // to normalize the alias after all.
                //
                // We handle this by lazily equating the alias and generalizing
                // it to an inference variable.
                let is_nested_alias = mem::replace(&mut self.in_alias, true);
                let result = match self.relate(data, data) {
                    Ok(data) => Ok(Ty::new_alias(self.tcx(), kind, data)),
                    Err(e) => {
                        if is_nested_alias {
                            return Err(e);
                        } else {
                            let mut visitor = MaxUniverse::new();
                            t.visit_with(&mut visitor);
                            let infer_replacement_is_complete =
                                self.for_universe.can_name(visitor.max_universe())
                                    && !t.has_escaping_bound_vars();
                            if !infer_replacement_is_complete {
                                warn!("may incompletely handle alias type: {t:?}");
                            }

                            debug!("generalization failure in alias");
                            Ok(self.infcx.next_ty_var_in_universe(
                                TypeVariableOrigin {
                                    kind: TypeVariableOriginKind::MiscVariable,
                                    span: self.delegate.span(),
                                },
                                self.for_universe,
                            ))
                        }
                    }
                };
                self.in_alias = is_nested_alias;
                result
            }

            _ => relate::structurally_relate_tys(self, t, t),
        }?;

        self.cache.insert(t, g);
        Ok(g)
    }

    #[instrument(level = "debug", skip(self, r2), ret)]
    fn regions(
        &mut self,
        r: ty::Region<'tcx>,
        r2: ty::Region<'tcx>,
    ) -> RelateResult<'tcx, ty::Region<'tcx>> {
        assert_eq!(r, r2); // we are misusing TypeRelation here; both LHS and RHS ought to be ==

        match *r {
            // Never make variables for regions bound within the type itself,
            // nor for erased regions.
            ty::ReBound(..) | ty::ReErased => {
                return Ok(r);
            }

            // It doesn't really matter for correctness if we generalize ReError,
            // since we're already on a doomed compilation path.
            ty::ReError(_) => {
                return Ok(r);
            }

            ty::RePlaceholder(..)
            | ty::ReVar(..)
            | ty::ReStatic
            | ty::ReEarlyParam(..)
            | ty::ReLateParam(..) => {
                // see common code below
            }
        }

        // If we are in an invariant context, we can re-use the region
        // as is, unless it happens to be in some universe that we
        // can't name.
        if let ty::Invariant = self.ambient_variance {
            let r_universe = self.infcx.universe_of_region(r);
            if self.for_universe.can_name(r_universe) {
                return Ok(r);
            }
        }

        Ok(self.delegate.generalize_region(self.for_universe))
    }

    #[instrument(level = "debug", skip(self, c2), ret)]
    fn consts(
        &mut self,
        c: ty::Const<'tcx>,
        c2: ty::Const<'tcx>,
    ) -> RelateResult<'tcx, ty::Const<'tcx>> {
        assert_eq!(c, c2); // we are misusing TypeRelation here; both LHS and RHS ought to be ==

        match c.kind() {
            ty::ConstKind::Infer(InferConst::Var(_)) if D::forbid_inference_vars() => {
                bug!("unexpected inference variable encountered in NLL generalization: {:?}", c);
            }
            ty::ConstKind::Infer(InferConst::Var(vid)) => {
                // If root const vids are equal, then `root_vid` and
                // `vid` are related and we'd be inferring an infinitely
                // deep const.
                if ty::TermVid::Const(
                    self.infcx.inner.borrow_mut().const_unification_table().find(vid).vid,
                ) == self.root_vid
                {
                    return Err(self.cyclic_term_error());
                }

                let mut inner = self.infcx.inner.borrow_mut();
                let variable_table = &mut inner.const_unification_table();
                match variable_table.probe_value(vid) {
                    ConstVariableValue::Known { value: u } => {
                        drop(inner);
                        self.relate(u, u)
                    }
                    ConstVariableValue::Unknown { origin, universe } => {
                        if self.for_universe.can_name(universe) {
                            Ok(c)
                        } else {
                            let new_var_id = variable_table
                                .new_key(ConstVariableValue::Unknown {
                                    origin,
                                    universe: self.for_universe,
                                })
                                .vid;
                            Ok(ty::Const::new_var(self.tcx(), new_var_id, c.ty()))
                        }
                    }
                }
            }
            ty::ConstKind::Infer(InferConst::EffectVar(_)) => Ok(c),
            // FIXME: remove this branch once `structurally_relate_consts` is fully
            // structural.
            ty::ConstKind::Unevaluated(ty::UnevaluatedConst { def, args }) => {
                let args = self.relate_with_variance(
                    ty::Variance::Invariant,
                    ty::VarianceDiagInfo::default(),
                    args,
                    args,
                )?;
                Ok(ty::Const::new_unevaluated(
                    self.tcx(),
                    ty::UnevaluatedConst { def, args },
                    c.ty(),
                ))
            }
            ty::ConstKind::Placeholder(placeholder) => {
                if self.for_universe.can_name(placeholder.universe) {
                    Ok(c)
                } else {
                    debug!(
                        "root universe {:?} cannot name placeholder in universe {:?}",
                        self.for_universe, placeholder.universe
                    );
                    Err(TypeError::Mismatch)
                }
            }
            _ => relate::structurally_relate_consts(self, c, c),
        }
    }

    #[instrument(level = "debug", skip(self), ret)]
    fn binders<T>(
        &mut self,
        a: ty::Binder<'tcx, T>,
        _: ty::Binder<'tcx, T>,
    ) -> RelateResult<'tcx, ty::Binder<'tcx, T>>
    where
        T: Relate<'tcx>,
    {
        let result = self.relate(a.skip_binder(), a.skip_binder())?;
        Ok(a.rebind(result))
    }
}

/// Result from a generalization operation. This includes
/// not only the generalized type, but also a bool flag
/// indicating whether further WF checks are needed.
#[derive(Debug)]
pub struct Generalization<T> {
    /// When generalizing `<?0 as Trait>::Assoc` or
    /// `<T as Bar<<?0 as Foo>::Assoc>>::Assoc`
    /// for `?0` generalization returns an inference
    /// variable.
    ///
    /// This has to be handled wotj care as it can
    /// otherwise very easily result in infinite
    /// recursion.
    pub value_may_be_infer: T,

    /// If true, then the generalized type may not be well-formed,
    /// even if the source type is well-formed, so we should add an
    /// additional check to enforce that it is. This arises in
    /// particular around 'bivariant' type parameters that are only
    /// constrained by a where-clause. As an example, imagine a type:
    ///
    ///     struct Foo<A, B> where A: Iterator<Item = B> {
    ///         data: A
    ///     }
    ///
    /// here, `A` will be covariant, but `B` is
    /// unconstrained. However, whatever it is, for `Foo` to be WF, it
    /// must be equal to `A::Item`. If we have an input `Foo<?A, ?B>`,
    /// then after generalization we will wind up with a type like
    /// `Foo<?C, ?D>`. When we enforce that `Foo<?A, ?B> <: Foo<?C,
    /// ?D>` (or `>:`), we will wind up with the requirement that `?A
    /// <: ?C`, but no particular relationship between `?B` and `?D`
    /// (after all, we do not know the variance of the normalized form
    /// of `A::Item` with respect to `A`). If we do nothing else, this
    /// may mean that `?D` goes unconstrained (as in #41677). So, in
    /// this scenario where we create a new type variable in a
    /// bivariant context, we set the `needs_wf` flag to true. This
    /// will force the calling code to check that `WF(Foo<?C, ?D>)`
    /// holds, which in turn implies that `?C::Item == ?D`. So once
    /// `?C` is constrained, that should suffice to restrict `?D`.
    pub needs_wf: bool,
}
