#[cfg(feature = "nightly")]
use rustc_data_structures::stable_hasher::{HashStable, StableHasher};
use std::fmt;

use crate::{DebruijnIndex, DebugWithInfcx, InferCtxtLike, Interner, WithInfcx};

use self::RegionKind::*;

/// Representation of regions. Note that the NLL checker uses a distinct
/// representation of regions. For this reason, it internally replaces all the
/// regions with inference variables -- the index of the variable is then used
/// to index into internal NLL data structures. See `rustc_const_eval::borrow_check`
/// module for more information.
///
/// Note: operations are on the wrapper `Region` type, which is interned,
/// rather than this type.
///
/// ## The Region lattice within a given function
///
/// In general, the region lattice looks like
///
/// ```text
/// static ----------+-----...------+       (greatest)
/// |                |              |
/// param regions    |              |
/// |                |              |
/// |                |              |
/// |                |              |
/// empty(root)   placeholder(U1)   |
/// |            /                  |
/// |           /         placeholder(Un)
/// empty(U1) --         /
/// |                   /
/// ...                /
/// |                 /
/// empty(Un) --------                      (smallest)
/// ```
///
/// Early-bound/free regions are the named lifetimes in scope from the
/// function declaration. They have relationships to one another
/// determined based on the declared relationships from the
/// function.
///
/// Note that inference variables and bound regions are not included
/// in this diagram. In the case of inference variables, they should
/// be inferred to some other region from the diagram. In the case of
/// bound regions, they are excluded because they don't make sense to
/// include -- the diagram indicates the relationship between free
/// regions.
///
/// ## Inference variables
///
/// During region inference, we sometimes create inference variables,
/// represented as `ReVar`. These will be inferred by the code in
/// `infer::lexical_region_resolve` to some free region from the
/// lattice above (the minimal region that meets the
/// constraints).
///
/// During NLL checking, where regions are defined differently, we
/// also use `ReVar` -- in that case, the index is used to index into
/// the NLL region checker's data structures. The variable may in fact
/// represent either a free region or an inference variable, in that
/// case.
///
/// ## Bound Regions
///
/// These are regions that are stored behind a binder and must be substituted
/// with some concrete region before being used. There are two kind of
/// bound regions: early-bound, which are bound in an item's `Generics`,
/// and are substituted by an `GenericArgs`, and late-bound, which are part of
/// higher-ranked types (e.g., `for<'a> fn(&'a ())`), and are substituted by
/// the likes of `liberate_late_bound_regions`. The distinction exists
/// because higher-ranked lifetimes aren't supported in all places. See [1][2].
///
/// Unlike `Param`s, bound regions are not supposed to exist "in the wild"
/// outside their binder, e.g., in types passed to type inference, and
/// should first be substituted (by placeholder regions, free regions,
/// or region variables).
///
/// ## Placeholder and Free Regions
///
/// One often wants to work with bound regions without knowing their precise
/// identity. For example, when checking a function, the lifetime of a borrow
/// can end up being assigned to some region parameter. In these cases,
/// it must be ensured that bounds on the region can't be accidentally
/// assumed without being checked.
///
/// To do this, we replace the bound regions with placeholder markers,
/// which don't satisfy any relation not explicitly provided.
///
/// There are two kinds of placeholder regions in rustc: `ReLateParam` and
/// `RePlaceholder`. When checking an item's body, `ReLateParam` is supposed
/// to be used. These also support explicit bounds: both the internally-stored
/// *scope*, which the region is assumed to outlive, as well as other
/// relations stored in the `FreeRegionMap`. Note that these relations
/// aren't checked when you `make_subregion` (or `eq_types`), only by
/// `resolve_regions_and_report_errors`.
///
/// When working with higher-ranked types, some region relations aren't
/// yet known, so you can't just call `resolve_regions_and_report_errors`.
/// `RePlaceholder` is designed for this purpose. In these contexts,
/// there's also the risk that some inference variable laying around will
/// get unified with your placeholder region: if you want to check whether
/// `for<'a> Foo<'_>: 'a`, and you substitute your bound region `'a`
/// with a placeholder region `'%a`, the variable `'_` would just be
/// instantiated to the placeholder region `'%a`, which is wrong because
/// the inference variable is supposed to satisfy the relation
/// *for every value of the placeholder region*. To ensure that doesn't
/// happen, you can use `leak_check`. This is more clearly explained
/// by the [rustc dev guide].
///
/// [1]: https://smallcultfollowing.com/babysteps/blog/2013/10/29/intermingled-parameter-lists/
/// [2]: https://smallcultfollowing.com/babysteps/blog/2013/11/04/intermingled-parameter-lists/
/// [rustc dev guide]: https://rustc-dev-guide.rust-lang.org/traits/hrtb.html
#[derive(derivative::Derivative)]
#[derivative(
    Clone(bound = ""),
    PartialOrd(bound = ""),
    PartialOrd = "feature_allow_slow_enum",
    Ord(bound = ""),
    Ord = "feature_allow_slow_enum",
    Hash(bound = "")
)]
#[cfg_attr(feature = "nightly", derive(TyEncodable, TyDecodable))]
pub enum RegionKind<I: Interner> {
    /// A region parameter; for example `'a` in `impl<'a> Trait for &'a ()`.
    ///
    /// There are some important differences between region and type parameters.
    /// Not all region parameters in the source are represented via `ReEarlyParam`:
    /// late-bound function parameters are instead lowered to a `ReBound`. Late-bound
    /// regions get eagerly replaced with `ReLateParam` which behaves in the same way as
    /// `ReEarlyParam`. Region parameters are also sometimes implicit,
    /// e.g. in `impl Trait for &()`.
    ReEarlyParam(I::EarlyParamRegion),

    /// A higher-ranked region. These represent either late-bound function parameters
    /// or bound variables from a `for<'a>`-binder.
    ///
    /// While inside of a function, e.g. during typeck, the late-bound function parameters
    /// can be converted to `ReLateParam` by calling `tcx.liberate_late_bound_regions`.
    ///
    /// Bound regions inside of types **must not** be erased, as they impact trait
    /// selection and the `TypeId` of that type. `for<'a> fn(&'a ())` and
    /// `fn(&'static ())` are different types and have to be treated as such.
    ReBound(DebruijnIndex, I::BoundRegion),

    /// Late-bound function parameters are represented using a `ReBound`. When
    /// inside of a function, we convert these bound variables to placeholder
    /// parameters via `tcx.liberate_late_bound_regions`. They are then treated
    /// the same way as `ReEarlyParam` while inside of the function.
    ///
    /// See <https://rustc-dev-guide.rust-lang.org/early-late-bound-summary.html> for
    /// more info about early and late bound lifetime parameters.
    ReLateParam(I::LateParamRegion),

    /// Static data that has an "infinite" lifetime. Top in the region lattice.
    ReStatic,

    /// A region variable. Should not exist outside of type inference.
    ReVar(I::InferRegion),

    /// A placeholder region -- the higher-ranked version of `ReLateParam`.
    /// Should not exist outside of type inference.
    ///
    /// Used when instantiating a `forall` binder via
    /// `infcx.instantiate_binder_with_placeholders`.
    RePlaceholder(I::PlaceholderRegion),

    /// Erased region, used by trait selection, in MIR and during codegen.
    ReErased,

    /// A region that resulted from some other error. Used exclusively for diagnostics.
    ReError(I::ErrorGuaranteed),
}

// This is manually implemented for `RegionKind` because `std::mem::discriminant`
// returns an opaque value that is `PartialEq` but not `PartialOrd`
#[inline]
const fn regionkind_discriminant<I: Interner>(value: &RegionKind<I>) -> usize {
    match value {
        ReEarlyParam(_) => 0,
        ReBound(_, _) => 1,
        ReLateParam(_) => 2,
        ReStatic => 3,
        ReVar(_) => 4,
        RePlaceholder(_) => 5,
        ReErased => 6,
        ReError(_) => 7,
    }
}

// This is manually implemented because a derive would require `I: Copy`
impl<I: Interner> Copy for RegionKind<I>
where
    I::EarlyParamRegion: Copy,
    I::BoundRegion: Copy,
    I::LateParamRegion: Copy,
    I::InferRegion: Copy,
    I::PlaceholderRegion: Copy,
    I::ErrorGuaranteed: Copy,
{
}

// This is manually implemented because a derive would require `I: PartialEq`
impl<I: Interner> PartialEq for RegionKind<I> {
    #[inline]
    fn eq(&self, other: &RegionKind<I>) -> bool {
        regionkind_discriminant(self) == regionkind_discriminant(other)
            && match (self, other) {
                (ReEarlyParam(a_r), ReEarlyParam(b_r)) => a_r == b_r,
                (ReBound(a_d, a_r), ReBound(b_d, b_r)) => a_d == b_d && a_r == b_r,
                (ReLateParam(a_r), ReLateParam(b_r)) => a_r == b_r,
                (ReStatic, ReStatic) => true,
                (ReVar(a_r), ReVar(b_r)) => a_r == b_r,
                (RePlaceholder(a_r), RePlaceholder(b_r)) => a_r == b_r,
                (ReErased, ReErased) => true,
                (ReError(_), ReError(_)) => true,
                _ => {
                    debug_assert!(
                        false,
                        "This branch must be unreachable, maybe the match is missing an arm? self = {self:?}, other = {other:?}"
                    );
                    true
                }
            }
    }
}

// This is manually implemented because a derive would require `I: Eq`
impl<I: Interner> Eq for RegionKind<I> {}

impl<I: Interner> DebugWithInfcx<I> for RegionKind<I> {
    fn fmt<Infcx: InferCtxtLike<Interner = I>>(
        this: WithInfcx<'_, Infcx, &Self>,
        f: &mut core::fmt::Formatter<'_>,
    ) -> core::fmt::Result {
        match this.data {
            ReEarlyParam(data) => write!(f, "ReEarlyParam({data:?})"),

            ReBound(binder_id, bound_region) => {
                write!(f, "ReBound({binder_id:?}, {bound_region:?})")
            }

            ReLateParam(fr) => write!(f, "{fr:?}"),

            ReStatic => f.write_str("ReStatic"),

            ReVar(vid) => write!(f, "{:?}", &this.wrap(vid)),

            RePlaceholder(placeholder) => write!(f, "RePlaceholder({placeholder:?})"),

            ReErased => f.write_str("ReErased"),

            ReError(_) => f.write_str("ReError"),
        }
    }
}
impl<I: Interner> fmt::Debug for RegionKind<I> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        WithInfcx::with_no_infcx(self).fmt(f)
    }
}

#[cfg(feature = "nightly")]
// This is not a derived impl because a derive would require `I: HashStable`
impl<CTX: crate::HashStableContext, I: Interner> HashStable<CTX> for RegionKind<I>
where
    I::EarlyParamRegion: HashStable<CTX>,
    I::BoundRegion: HashStable<CTX>,
    I::LateParamRegion: HashStable<CTX>,
    I::InferRegion: HashStable<CTX>,
    I::PlaceholderRegion: HashStable<CTX>,
{
    #[inline]
    fn hash_stable(&self, hcx: &mut CTX, hasher: &mut StableHasher) {
        std::mem::discriminant(self).hash_stable(hcx, hasher);
        match self {
            ReErased | ReStatic | ReError(_) => {
                // No variant fields to hash for these ...
            }
            ReBound(d, r) => {
                d.hash_stable(hcx, hasher);
                r.hash_stable(hcx, hasher);
            }
            ReEarlyParam(r) => {
                r.hash_stable(hcx, hasher);
            }
            ReLateParam(r) => {
                r.hash_stable(hcx, hasher);
            }
            RePlaceholder(r) => {
                r.hash_stable(hcx, hasher);
            }
            ReVar(_) => {
                panic!("region variables should not be hashed: {self:?}")
            }
        }
    }
}
