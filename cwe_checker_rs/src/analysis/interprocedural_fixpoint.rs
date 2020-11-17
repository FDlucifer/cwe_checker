//! Creating and computing interprocedural fixpoint problems.
//!
//! # General notes
//!
//! This module supports computation of fixpoint problems on the control flow graphs generated by the `graph` module.
//! As of this writing, only forward analyses are possible,
//! backward analyses are not yet implemented.
//!
//! To compute a generalized fixpoint problem,
//! first construct a context object implementing the `Context`trait.
//! Use it to construct a `Computation` object.
//! The `Computation` object provides the necessary methods for the actual fixpoint computation.

use super::fixpoint::Context as GeneralFPContext;
use super::graph::*;
use crate::intermediate_representation::*;
use crate::prelude::*;
use fnv::FnvHashMap;
use petgraph::graph::{EdgeIndex, NodeIndex};
use std::marker::PhantomData;

#[derive(PartialEq, Eq, Serialize, Deserialize)]
pub enum NodeValue<T: PartialEq + Eq> {
    Value(T),
    CallReturnCombinator { call: Option<T>, return_: Option<T> },
}

impl<T: PartialEq + Eq> NodeValue<T> {
    pub fn unwrap_value(&self) -> &T {
        match self {
            NodeValue::Value(value) => value,
            _ => panic!("Unexpected node value type"),
        }
    }
}

/// The context for an interprocedural fixpoint computation.
///
/// Basically, a `Context` object needs to contain a reference to the actual graph,
/// a method for merging node values,
/// and methods for computing the edge transitions for each different edge type.
///
/// All trait methods have access to the FixpointProblem structure, so that context informations are accessible through it.
///
/// All edge transition functions can return `None` to indicate that no information flows through the edge.
/// For example, this can be used to indicate edges that can never been taken.
pub trait Context<'a> {
    type Value: PartialEq + Eq + Clone;

    /// Get a reference to the graph that the fixpoint is computed on.
    fn get_graph(&self) -> &Graph<'a>;

    /// Merge two node values.
    fn merge(&self, value1: &Self::Value, value2: &Self::Value) -> Self::Value;

    /// Transition function for `Def` terms.
    /// The transition function for a basic block is computed
    /// by iteratively applying this function to the starting value for each `Def` term in the basic block.
    /// The iteration short-circuits and returns `None` if `update_def` returns `None` at any point.
    fn update_def(&self, value: &Self::Value, def: &Term<Def>) -> Option<Self::Value>;

    /// Transition function for (conditional and unconditional) `Jmp` terms.
    fn update_jump(
        &self,
        value: &Self::Value,
        jump: &Term<Jmp>,
        untaken_conditional: Option<&Term<Jmp>>,
        target: &Term<Blk>,
    ) -> Option<Self::Value>;

    /// Transition function for in-program calls.
    fn update_call(
        &self,
        value: &Self::Value,
        call: &Term<Jmp>,
        target: &Node,
    ) -> Option<Self::Value>;

    /// Transition function for return instructions.
    /// Has access to the value at the callsite corresponding to the return edge.
    /// This way one can recover caller-specific information on return from a function.
    fn update_return(
        &self,
        value: Option<&Self::Value>,
        value_before_call: Option<&Self::Value>,
        call_term: &Term<Jmp>,
        return_term: &Term<Jmp>,
    ) -> Option<Self::Value>;

    /// Transition function for calls to functions not contained in the binary.
    /// The corresponding edge goes from the callsite to the returned-to block.
    fn update_call_stub(&self, value: &Self::Value, call: &Term<Jmp>) -> Option<Self::Value>;

    /// This function is used to refine the value using the information on which branch was taken on a conditional jump.
    fn specialize_conditional(
        &self,
        value: &Self::Value,
        condition: &Expression,
        is_true: bool,
    ) -> Option<Self::Value>;
}

/// This struct is a wrapper to create a general fixpoint context out of an interprocedural fixpoint context.
struct GeneralizedContext<'a, T: Context<'a>> {
    context: T,
    _phantom_graph_reference: PhantomData<Graph<'a>>,
}

impl<'a, T: Context<'a>> GeneralizedContext<'a, T> {
    /// Create a new generalized context out of an interprocedural context object.
    pub fn new(context: T) -> Self {
        GeneralizedContext {
            context,
            _phantom_graph_reference: PhantomData,
        }
    }
}

impl<'a, T: Context<'a>> GeneralFPContext for GeneralizedContext<'a, T> {
    type EdgeLabel = Edge<'a>;
    type NodeLabel = Node<'a>;
    type NodeValue = NodeValue<T::Value>;

    /// Get a reference to the underlying graph.
    fn get_graph(&self) -> &Graph<'a> {
        self.context.get_graph()
    }

    /// Merge two values using the merge function from the interprocedural context object.
    fn merge(&self, val1: &Self::NodeValue, val2: &Self::NodeValue) -> Self::NodeValue {
        use NodeValue::*;
        match (val1, val2) {
            (Value(value1), Value(value2)) => Value(self.context.merge(value1, value2)),
            (
                CallReturnCombinator {
                    call: call1,
                    return_: return1,
                },
                CallReturnCombinator {
                    call: call2,
                    return_: return2,
                },
            ) => CallReturnCombinator {
                call: merge_option(call1, call2, |v1, v2| self.context.merge(v1, v2)),
                return_: merge_option(return1, return2, |v1, v2| self.context.merge(v1, v2)),
            },
            _ => panic!("Malformed CFG in fixpoint computation"),
        }
    }

    /// Edge transition function.
    /// Applies the transition functions from the interprocedural context object
    /// corresponding to the type of the provided edge.
    fn update_edge(
        &self,
        node_value: &Self::NodeValue,
        edge: EdgeIndex,
    ) -> Option<Self::NodeValue> {
        let graph = self.context.get_graph();
        let (start_node, end_node) = graph.edge_endpoints(edge).unwrap();

        match graph.edge_weight(edge).unwrap() {
            Edge::Block => {
                let block_term = graph.node_weight(start_node).unwrap().get_block();
                let value = node_value.unwrap_value();
                let defs = &block_term.term.defs;
                let end_val = defs.iter().try_fold(value.clone(), |accum, def| {
                    self.context.update_def(&accum, def)
                });
                end_val.map(NodeValue::Value)
            }
            Edge::Call(call) => self
                .context
                .update_call(node_value.unwrap_value(), call, &graph[end_node])
                .map(NodeValue::Value),
            Edge::CRCallStub => Some(NodeValue::CallReturnCombinator {
                call: Some(node_value.unwrap_value().clone()),
                return_: None,
            }),
            Edge::CRReturnStub => Some(NodeValue::CallReturnCombinator {
                call: None,
                return_: Some(node_value.unwrap_value().clone()),
            }),
            Edge::CRCombine(call_term) => match node_value {
                NodeValue::Value(_) => panic!("Unexpected interprocedural fixpoint graph state"),
                NodeValue::CallReturnCombinator { call, return_ } => {
                    let return_from_block = match graph.node_weight(start_node) {
                        Some(Node::CallReturn {
                            call: _,
                            return_: (return_from_block, _),
                        }) => return_from_block,
                        _ => panic!("Malformed Control flow graph"),
                    };
                    let return_from_jmp = &return_from_block.term.jmps[0];
                    match self.context.update_return(
                        return_.as_ref(),
                        call.as_ref(),
                        call_term,
                        return_from_jmp,
                    ) {
                        Some(val) => Some(NodeValue::Value(val)),
                        None => None,
                    }
                }
            },
            Edge::ExternCallStub(call) => self
                .context
                .update_call_stub(node_value.unwrap_value(), call)
                .map(NodeValue::Value),
            Edge::Jump(jump, untaken_conditional) => self
                .context
                .update_jump(
                    node_value.unwrap_value(),
                    jump,
                    *untaken_conditional,
                    graph[end_node].get_block(),
                )
                .map(NodeValue::Value),
        }
    }
}

/// An intermediate result of an interprocedural fixpoint computation.
///
/// The usage instructions are identical to the usage of the general fixpoint computation object,
/// except that you need to provide an interprocedural context object instead of a general one.
pub struct Computation<'a, T: Context<'a>> {
    generalized_computation: super::fixpoint::Computation<GeneralizedContext<'a, T>>,
}

impl<'a, T: Context<'a>> Computation<'a, T> {
    /// Generate a new computation from the corresponding context and an optional default value for nodes.
    pub fn new(problem: T, default_value: Option<T::Value>) -> Self {
        let generalized_problem = GeneralizedContext::new(problem);
        let computation = super::fixpoint::Computation::new(
            generalized_problem,
            default_value.map(NodeValue::Value),
        );
        Computation {
            generalized_computation: computation,
        }
    }

    /// Compute the fixpoint.
    /// Note that this function does not terminate if the fixpoint algorithm does not stabilize.
    pub fn compute(&mut self) {
        self.generalized_computation.compute()
    }

    /// Compute the fixpoint while updating each node at most max_steps times.
    /// Note that the result may not be a stabilized fixpoint, but only an intermediate result of a fixpoint computation.
    pub fn compute_with_max_steps(&mut self, max_steps: u64) {
        self.generalized_computation
            .compute_with_max_steps(max_steps)
    }

    /// Get the value of a node.
    pub fn get_node_value(&self, node: NodeIndex) -> Option<&NodeValue<T::Value>> {
        self.generalized_computation.get_node_value(node)
    }

    /// Set the value of a node and mark the node as not yet stabilized
    pub fn set_node_value(&mut self, node: NodeIndex, value: NodeValue<T::Value>) {
        self.generalized_computation.set_node_value(node, value)
    }

    /// Get a reference to the internal map where one can look up the current values of all nodes
    pub fn node_values(&self) -> &FnvHashMap<NodeIndex, NodeValue<T::Value>> {
        self.generalized_computation.node_values()
    }

    /// Get a reference to the underlying graph
    pub fn get_graph(&self) -> &Graph {
        self.generalized_computation.get_graph()
    }

    /// Get a reference to the underlying context object
    pub fn get_context(&self) -> &T {
        &self.generalized_computation.get_context().context
    }

    /// Returns `True` if the computation has stabilized, i.e. the internal worklist is empty.
    pub fn has_stabilized(&self) -> bool {
        self.generalized_computation.has_stabilized()
    }
}

/// Helper function to merge to values wrapped in `Option<..>`.
/// Merges `(Some(x), None)` to `Some(x)`.
fn merge_option<T: Clone, F>(opt1: &Option<T>, opt2: &Option<T>, merge: F) -> Option<T>
where
    F: Fn(&T, &T) -> T,
{
    match (opt1, opt2) {
        (Some(value1), Some(value2)) => Some(merge(value1, value2)),
        (Some(value), None) | (None, Some(value)) => Some(value.clone()),
        (None, None) => None,
    }
}
