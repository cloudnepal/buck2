/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under both the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree and the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree.
 */

use std::fmt;
use std::iter;
use std::sync::Arc;

use allocative::Allocative;
use anyhow::Context;
use buck2_error::BuckErrorContext;
use buck2_interpreter::error::BuckStarlarkError;
use buck2_interpreter::error::OtherErrorHandling;
use display_container::display_pair;
use display_container::fmt_container;
use display_container::iter_display_chain;
use dupe::Dupe;
use either::Either;
use gazebo::prelude::*;
use serde::ser::SerializeMap;
use serde::Serialize;
use serde::Serializer;
use starlark::any::ProvidesStaticType;
use starlark::coerce::Coerce;
use starlark::environment::Methods;
use starlark::environment::MethodsBuilder;
use starlark::environment::MethodsStatic;
use starlark::eval::Evaluator;
use starlark::values::list::AllocList;
use starlark::values::starlark_value;
use starlark::values::typing::TypeInstanceId;
use starlark::values::typing::TypeMatcher;
use starlark::values::Freeze;
use starlark::values::Freezer;
use starlark::values::FrozenValue;
use starlark::values::FrozenValueTyped;
use starlark::values::StarlarkValue;
use starlark::values::Trace;
use starlark::values::UnpackValue;
use starlark::values::Value;
use starlark::values::ValueLifetimeless;
use starlark::values::ValueLike;
use starlark::values::ValueOf;
use starlark::values::ValueTypedComplex;

use crate::actions::impls::json::validate_json;
use crate::actions::impls::json::visit_json_artifacts;
use crate::actions::impls::json::JsonUnpack;
use crate::artifact_groups::deferred::TransitiveSetKey;
use crate::artifact_groups::ArtifactGroup;
use crate::artifact_groups::TransitiveSetProjectionKey;
use crate::interpreter::rule_defs::cmd_args::SimpleCommandLineArtifactVisitor;
use crate::interpreter::rule_defs::transitive_set::transitive_set_definition::TransitiveSetDefinitionLike;
use crate::interpreter::rule_defs::transitive_set::transitive_set_definition::TransitiveSetProjectionKind;
use crate::interpreter::rule_defs::transitive_set::traversal::TransitiveSetOrdering;
use crate::interpreter::rule_defs::transitive_set::traversal::TransitiveSetTraversal;
use crate::interpreter::rule_defs::transitive_set::BfsTransitiveSetIteratorGen;
use crate::interpreter::rule_defs::transitive_set::FrozenTransitiveSetDefinition;
use crate::interpreter::rule_defs::transitive_set::PostorderTransitiveSetIteratorGen;
use crate::interpreter::rule_defs::transitive_set::PreorderTransitiveSetIteratorGen;
use crate::interpreter::rule_defs::transitive_set::TopologicalTransitiveSetIteratorGen;
use crate::interpreter::rule_defs::transitive_set::TransitiveSetArgsProjection;
use crate::interpreter::rule_defs::transitive_set::TransitiveSetDefinition;
use crate::interpreter::rule_defs::transitive_set::TransitiveSetError;
use crate::interpreter::rule_defs::transitive_set::TransitiveSetIteratorLike;
use crate::interpreter::rule_defs::transitive_set::TransitiveSetJsonProjection;

#[derive(Clone, Debug, Allocative)]
pub(crate) struct TransitiveSetMatcher {
    pub(crate) type_instance_id: TypeInstanceId,
}

impl TypeMatcher for TransitiveSetMatcher {
    fn matches(&self, value: Value) -> bool {
        let Some(tset) = ValueTypedComplex::<TransitiveSet>::new(value) else {
            return false;
        };
        let tset_definition: Value = match tset.unpack() {
            Either::Left(tset) => tset.definition.to_value(),
            Either::Right(tset) => tset.definition.to_value(),
        };
        let tset_definition = ValueTypedComplex::<TransitiveSetDefinition>::new(tset_definition)
            .expect("wrong type of definition");
        let exported = match tset_definition.unpack() {
            Either::Left(definition) => match definition.exported.get() {
                Some(definition) => definition,
                None => return false,
            },
            Either::Right(definition) => &definition.exported,
        };
        // TODO(nga): suboptimal: we could just compare to the pointer of the definition.
        exported.set_type_instance_id == self.type_instance_id
    }
}

#[derive(Debug, Clone, Trace, ProvidesStaticType, Allocative)]
#[repr(C)]
pub struct TransitiveSetGen<V: ValueLifetimeless> {
    /// A Deferred key that maps back to this set. This is used to compute its inputs.
    pub key: TransitiveSetKey,

    /// The TransitiveSetCallable that this set uses.
    pub(crate) definition: FrozenValueTyped<'static, FrozenTransitiveSetDefinition>,

    /// The immediate value of this node. If None, then this node will not yield anything when
    /// iterated over (but we'll still traverse to its children).
    pub(crate) node: Option<NodeGen<V>>,

    /// Pre-computed reductions. Those are arbitrary values based on the set's definition.
    pub(crate) reductions: Box<[V]>,

    /// Further transitive sets.
    pub children: Box<[V]>,
}

#[derive(Debug, Clone, Trace, Allocative)]
#[repr(C)]
pub struct NodeGen<V: ValueLifetimeless> {
    /// The value
    pub value: V,

    /// Pre-computed projections.
    pub projections: Box<[V]>,
}

unsafe impl<'v> Coerce<TransitiveSetGen<Value<'v>>> for TransitiveSetGen<FrozenValue> {}

impl<V: ValueLifetimeless> fmt::Display for TransitiveSetGen<V> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        fmt_container(
            f,
            &format!("{}(", self.definition),
            ")",
            iter_display_chain(
                self.node
                    .as_ref()
                    .map(|node| display_pair("value", "=", &node.value)),
                iter::once(format!("{} children", self.children.len())),
            ),
        )
    }
}

impl<'v, V: ValueLike<'v>> Serialize for TransitiveSetGen<V> {
    fn serialize<S>(&self, s: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut s = s.serialize_map(Some(3))?;
        s.serialize_entry("definition", &self.definition)?;
        if let Some(node) = self.node.as_ref() {
            s.serialize_entry("value", &node.value)?;
        }

        s.serialize_entry("children", &self.children.len())?;
        s.end()
    }
}

impl<V: ValueLifetimeless> TransitiveSetGen<V> {
    pub fn key(&self) -> &TransitiveSetKey {
        &self.key
    }
}

impl<'v> NodeGen<Value<'v>> {
    fn freeze(self, freezer: &Freezer) -> anyhow::Result<NodeGen<FrozenValue>> {
        let Self { value, projections } = self;

        let value = value.freeze(freezer)?;
        let projections = projections.freeze(freezer)?;

        Ok(NodeGen { value, projections })
    }
}

impl<'v, V: ValueLike<'v>> TransitiveSetGen<V> {
    fn matches_definition(
        &self,
        definition: FrozenValueTyped<'v, FrozenTransitiveSetDefinition>,
    ) -> bool {
        definition.to_value().ptr_eq(self.definition.to_value())
    }

    pub fn projection_name(&'v self, projection: usize) -> anyhow::Result<&'v str> {
        let def = self.definition.as_ref();

        Ok(def
            .operations()
            .projections
            .get_index(projection)
            .context("Invalid projection id")?
            .0
            .as_str())
    }

    pub fn get_projection_value(&self, projection: usize) -> anyhow::Result<Option<V>> {
        match &self.node {
            None => Ok(None),
            Some(node) => Ok(Some(
                *node
                    .projections
                    .get(projection)
                    .context("Invalid projection id")?,
            )),
        }
    }

    pub fn get_projection_key(&self, projection: usize) -> TransitiveSetProjectionKey {
        TransitiveSetProjectionKey {
            key: self.key.dupe(),
            projection,
        }
    }

    pub(crate) fn definition(
        &self,
    ) -> anyhow::Result<ValueTypedComplex<'v, TransitiveSetDefinition<'v>>> {
        ValueTypedComplex::unpack_value_err(self.definition.to_value())
            .internal_error_anyhow("Must be a TransitiveSetDefinition")
    }
}

impl FrozenTransitiveSet {
    pub fn get_projection_sub_inputs(
        &self,
        projection: usize,
    ) -> anyhow::Result<Vec<ArtifactGroup>> {
        let mut sub_inputs = Vec::new();

        if let Some(projection) = self.get_projection_value(projection)? {
            let mut visitor = SimpleCommandLineArtifactVisitor::new();
            // It's either an args-like or a json projection. visit_json_artifacts handles both the way we want.
            visit_json_artifacts(projection.to_value(), &mut visitor)?;
            sub_inputs.extend(visitor.inputs);
        }

        // Reuse the same projection for children sets.
        for v in self.children.iter() {
            let v = TransitiveSet::from_value(v.to_value()).context("Invalid deferred")?;
            sub_inputs.push(ArtifactGroup::TransitiveSetProjection(Arc::new(
                TransitiveSetProjectionKey {
                    key: v.key().dupe(),
                    projection,
                },
            )));
        }
        Ok(sub_inputs)
    }
}

impl<'v, V> TransitiveSetGen<V>
where
    V: ValueLike<'v>,
    TransitiveSetGen<V>: StarlarkValue<'v> + TransitiveSetLike<'v>,
{
    pub fn iter<'a>(
        &'a self,
        ordering: TransitiveSetOrdering,
    ) -> Box<dyn TransitiveSetIteratorLike<'a, 'v, V> + 'a>
    where
        'v: 'a,
    {
        match ordering {
            TransitiveSetOrdering::Preorder => {
                Box::new(PreorderTransitiveSetIteratorGen::new(self))
            }
            TransitiveSetOrdering::Postorder => {
                Box::new(PostorderTransitiveSetIteratorGen::new(self))
            }
            TransitiveSetOrdering::Topological => {
                Box::new(TopologicalTransitiveSetIteratorGen::new(self))
            }
            TransitiveSetOrdering::Bfs => Box::new(BfsTransitiveSetIteratorGen::new(self)),
        }
    }

    pub fn iter_values<'a>(
        &'a self,
        ordering: TransitiveSetOrdering,
    ) -> anyhow::Result<Box<dyn Iterator<Item = Value<'v>> + 'a>>
    where
        'v: 'a,
    {
        Ok(Box::new(
            self.iter(ordering)
                .values()
                .map(|node| node.value.to_value()),
        ))
    }

    pub(super) fn iter_projection_values<'a>(
        &'a self,
        ordering: TransitiveSetOrdering,
        projection: usize,
    ) -> anyhow::Result<Box<dyn Iterator<Item = Value<'v>> + 'a>>
    where
        'v: 'a,
    {
        let mut iter = self.iter(ordering).values().peekable();

        // Defensively, check the projection is valid. We know the set has the same definition
        // throughout so it'll be safe (enough) to unwrap if it is valid on the first one.
        if let Some(v) = iter.peek() {
            v.projections
                .get(projection)
                .context("Invalid projection")?;
        }

        Ok(Box::new(iter.map(move |node| {
            node.projections.get(projection).unwrap().to_value()
        })))
    }
}

pub trait TransitiveSetLike<'v> {
    fn from_value(v: Value<'v>) -> Option<&'v Self>;
}

impl<'v> TransitiveSetLike<'v> for TransitiveSet<'v> {
    fn from_value(v: Value<'v>) -> Option<&'v Self> {
        TransitiveSet::from_value(v)
    }
}

impl<'v> TransitiveSetLike<'v> for FrozenTransitiveSet {
    fn from_value(v: Value<'v>) -> Option<&'v Self> {
        // FrozenTransitiveSet::from_value(v)

        v.downcast_ref::<FrozenTransitiveSet>()
    }
}

starlark_complex_value!(pub TransitiveSet);

#[starlark_value(type = "transitive_set")]
impl<'v, V: ValueLike<'v>> StarlarkValue<'v> for TransitiveSetGen<V>
where
    Self: ProvidesStaticType<'v> + TransitiveSetLike<'v>,
{
    fn get_methods() -> Option<&'static Methods> {
        static RES: MethodsStatic = MethodsStatic::new();
        RES.methods(transitive_set_methods)
    }
}

impl<'v> Freeze for TransitiveSet<'v> {
    type Frozen = FrozenTransitiveSet;

    fn freeze(self, freezer: &Freezer) -> anyhow::Result<Self::Frozen> {
        let Self {
            key,
            definition,
            node,
            reductions,
            children,
        } = self;
        let definition = definition.freeze(freezer)?;
        let node = node.try_map(|node| node.freeze(freezer))?;
        let children = children.freeze(freezer)?;
        let reductions = reductions.freeze(freezer)?;
        Ok(TransitiveSetGen {
            key,
            definition,
            node,
            reductions,
            children,
        })
    }
}

impl<'v> TransitiveSet<'v> {
    pub fn new(
        key: TransitiveSetKey,
        definition: FrozenValueTyped<'v, FrozenTransitiveSetDefinition>,
        value: Option<Value<'v>>,
        children: impl IntoIterator<Item = Value<'v>>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> anyhow::Result<Self> {
        let def: &dyn TransitiveSetDefinitionLike = &*definition;
        if !def.has_id() {
            return Err(TransitiveSetError::TransitiveSetUsedBeforeAssignment.into());
        }

        let children = children.into_iter().collect::<Box<[_]>>();
        let children_sets = children.try_map(|v| match TransitiveSet::from_value(*v) {
            Some(set) if set.matches_definition(definition) => Ok(set),
            Some(set) => {
                fn format_def(def: &FrozenTransitiveSetDefinition) -> String {
                    format!("{:?}", def.as_debug())
                }
                Err(TransitiveSetError::TransitiveValueIsOfWrongType {
                    expected: format_def(&definition),
                    got: format_def(&set.definition),
                })
            }
            None => {
                Err(TransitiveSetError::TransitiveValueIsNotTransitiveSet { got: v.to_string() })
            }
        })?;

        let node = value.try_map(|value| {
            let projections = def
                .operations()
                .projections
                .iter()
                .map(|(name, spec)| {
                    let projected_value = eval
                        .eval_function(spec.projection.get(), &[value], &[])
                        .map_err(|error| TransitiveSetError::ProjectionError {
                            error: BuckStarlarkError::new(error, OtherErrorHandling::InputError)
                                .into(),
                            name: name.clone(),
                        })?;
                    match spec.kind {
                        TransitiveSetProjectionKind::Args => {
                            TransitiveSetArgsProjection::as_command_line(projected_value)?;
                        }
                        TransitiveSetProjectionKind::Json => {
                            validate_json(JsonUnpack::unpack_value_err(projected_value)?)?;
                        }
                    }
                    anyhow::Ok(projected_value)
                })
                .collect::<Result<Box<[_]>, _>>()?;

            anyhow::Ok(NodeGen { value, projections })
        })?;

        let reductions = def
            .operations()
            .reductions
            .iter()
            .enumerate()
            .map(|(idx, (name, reduce))| {
                let children_values = children_sets.try_map(|c| {
                    c.reductions
                        .get(idx)
                        .copied()
                        .with_context(|| format!("Child {} is missing reduction {}", c, idx))
                })?;
                let children_values = eval.heap().alloc(AllocList(children_values));

                let value = value.unwrap_or_else(Value::new_none);

                let reduced = eval
                    .eval_function(*reduce, &[children_values, value], &[])
                    .map_err(|error| TransitiveSetError::ReductionError {
                        error: BuckStarlarkError::new(error, OtherErrorHandling::InputError).into(),
                        name: name.clone(),
                    })?;

                anyhow::Ok(reduced)
            })
            .collect::<Result<Box<[_]>, _>>()?;

        Ok(Self {
            key,
            definition:
            // Cast lifetime from 'v to 'static
            FrozenValueTyped::<FrozenTransitiveSetDefinition>::new(FrozenValueTyped::<FrozenTransitiveSetDefinition>::to_frozen_value(definition)).context("internal error")?,
            node,
            reductions,
            children,
        })
    }

    pub fn new_from_values(
        key: TransitiveSetKey,
        definition: FrozenValueTyped<'v, FrozenTransitiveSetDefinition>,
        value: Option<Value<'v>>,
        children: Option<Value<'v>>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<Self> {
        let children = children
            .map(|v| v.iterate(eval.heap()))
            .transpose()?
            .into_iter()
            .flatten();

        Self::new(key, definition, value, children, eval).map_err(Into::into)
    }
}

#[starlark_module]
fn transitive_set_methods(builder: &mut MethodsBuilder) {
    fn project_as_json<'v>(
        this: ValueOf<'v, &'v TransitiveSet<'v>>,
        projection: &str,
        #[starlark(require = named, default = "preorder")] ordering: &str,
    ) -> anyhow::Result<TransitiveSetJsonProjection<'v>> {
        let def = this.typed.definition;

        let index = def
            .operations()
            .get_index_of_projection(TransitiveSetProjectionKind::Json, projection)?;

        Ok(TransitiveSetJsonProjection {
            transitive_set: this.value,
            projection: index,
            ordering: TransitiveSetOrdering::parse(ordering)?,
        })
    }

    fn project_as_args<'v>(
        this: ValueOf<'v, &'v TransitiveSet<'v>>,
        projection: &str,
        #[starlark(require = named, default = "preorder")] ordering: &str,
    ) -> anyhow::Result<TransitiveSetArgsProjection<'v>> {
        let def = this.typed.definition;

        let index = def
            .operations()
            .get_index_of_projection(TransitiveSetProjectionKind::Args, projection)?;

        Ok(TransitiveSetArgsProjection {
            transitive_set: this.value,
            projection: index,
            ordering: TransitiveSetOrdering::parse(ordering)?,
        })
    }

    fn reduce<'v>(
        this: ValueOf<'v, &'v TransitiveSet<'v>>,
        reduction: &str,
    ) -> anyhow::Result<Value<'v>> {
        let def = this.typed.definition;

        let index = match def.operations().reductions.get_index_of(reduction) {
            Some(index) => index,
            None => {
                return Err(TransitiveSetError::ReductionDoesNotExist {
                    reduction: reduction.into(),
                    valid_reductions: def
                        .operations()
                        .reductions
                        .keys()
                        .map(String::from)
                        .collect::<Vec<_>>(),
                }
                .into());
            }
        };

        this.typed
            .reductions
            .get(index)
            .copied()
            .with_context(|| format!("Missing reduction {}", index))
    }

    fn traverse<'v>(
        this: ValueOf<'v, &'v TransitiveSet<'v>>,
        #[starlark(require = named, default = "preorder")] ordering: &str,
    ) -> anyhow::Result<TransitiveSetTraversal<'v>> {
        Ok(TransitiveSetTraversal {
            inner: this.value,
            ordering: TransitiveSetOrdering::parse(ordering)?,
        })
    }

    #[starlark(attribute)]
    fn definition<'v>(
        this: ValueOf<'v, &'v TransitiveSet<'v>>,
    ) -> anyhow::Result<ValueTypedComplex<'v, TransitiveSetDefinition<'v>>> {
        this.typed.definition()
    }

    #[starlark(attribute)]
    fn value<'v>(this: ValueOf<'v, &'v TransitiveSet<'v>>) -> anyhow::Result<Value<'v>> {
        Ok(match this.typed.node.as_ref() {
            Some(node) => node.value,
            None => Value::new_none(),
        })
    }
    #[starlark(attribute)]
    fn children<'v>(this: ValueOf<'v, &'v TransitiveSet<'v>>) -> anyhow::Result<Vec<Value<'v>>> {
        Ok(this.typed.children.to_vec())
    }
}
