//! A SSA-building API that handles incomplete CFGs.
//!
//! The algorithm is based upon Braun M., Buchwald S., Hack S., Leißa R., Mallon C.,
//! Zwinkau A. (2013) Simple and Efficient Construction of Static Single Assignment Form.
//! In: Jhala R., De Bosschere K. (eds) Compiler Construction. CC 2013.
//! Lecture Notes in Computer Science, vol 7791. Springer, Berlin, Heidelberg
//!
//! <https://link.springer.com/content/pdf/10.1007/978-3-642-37051-9_6.pdf>

use crate::Variable;
use alloc::vec::Vec;
use core::convert::TryInto;
use core::mem;
use cranelift_codegen::cursor::{Cursor, FuncCursor};
use cranelift_codegen::entity::{EntitySet, SecondaryMap};
use cranelift_codegen::ir::immediates::{Ieee32, Ieee64};
use cranelift_codegen::ir::instructions::BranchInfo;
use cranelift_codegen::ir::types::{F32, F64};
use cranelift_codegen::ir::{
    Block, Function, Inst, InstBuilder, InstructionData, JumpTableData, Type, Value,
};
use cranelift_codegen::packed_option::PackedOption;
use smallvec::SmallVec;

/// Structure containing the data relevant the construction of SSA for a given function.
///
/// The parameter struct `Variable` corresponds to the way variables are represented in the
/// non-SSA language you're translating from.
///
/// The SSA building relies on information about the variables used and defined.
///
/// This SSA building module allows you to def and use variables on the fly while you are
/// constructing the CFG, no need for a separate SSA pass after the CFG is completed.
///
/// A basic block is said _filled_ if all the instruction that it contains have been translated,
/// and it is said _sealed_ if all of its predecessors have been declared. Only filled predecessors
/// can be declared.
#[derive(Default)]
pub struct SSABuilder {
    // TODO: Consider a sparse representation rather than SecondaryMap-of-SecondaryMap.
    /// Records for every variable and for every relevant block, the last definition of
    /// the variable in the block.
    variables: SecondaryMap<Variable, SecondaryMap<Block, PackedOption<Value>>>,

    /// Records the position of the basic blocks and the list of values used but not defined in the
    /// block.
    ssa_blocks: SecondaryMap<Block, SSABlockData>,

    /// Call stack for use in the `use_var`/`predecessors_lookup` state machine.
    calls: Vec<Call>,
    /// Result stack for use in the `use_var`/`predecessors_lookup` state machine.
    results: Vec<Value>,

    /// Side effects accumulated in the `use_var`/`predecessors_lookup` state machine.
    side_effects: SideEffects,

    /// Reused storage for cycle-detection.
    visited: EntitySet<Block>,
}

/// Side effects of a `use_var` or a `seal_block` method call.
#[derive(Default)]
pub struct SideEffects {
    /// When we want to append jump arguments to a `br_table` instruction, the critical edge is
    /// splitted and the newly created `Block`s are signaled here.
    pub split_blocks_created: Vec<Block>,
    /// When a variable is used but has never been defined before (this happens in the case of
    /// unreachable code), a placeholder `iconst` or `fconst` value is added to the right `Block`.
    /// This field signals if it is the case and return the `Block` to which the initialization has
    /// been added.
    pub instructions_added_to_blocks: Vec<Block>,
}

impl SideEffects {
    fn is_empty(&self) -> bool {
        self.split_blocks_created.is_empty() && self.instructions_added_to_blocks.is_empty()
    }
}

#[derive(Clone)]
struct PredBlock {
    block: Block,
    branch: Inst,
}

impl PredBlock {
    fn new(block: Block, branch: Inst) -> Self {
        Self { block, branch }
    }
}

#[derive(Clone)]
enum Sealed {
    No {
        // List of current Block arguments for which an earlier def has not been found yet.
        undef_variables: Vec<(Variable, Value)>,
    },
    Yes,
}

impl Default for Sealed {
    fn default() -> Self {
        Sealed::No {
            undef_variables: Vec::new(),
        }
    }
}

type PredBlockSmallVec = SmallVec<[PredBlock; 4]>;

#[derive(Clone, Default)]
struct SSABlockData {
    // The predecessors of the Block with the block and branch instruction.
    predecessors: PredBlockSmallVec,
    // A block is sealed if all of its predecessors have been declared.
    sealed: Sealed,
}

impl SSABlockData {
    fn add_predecessor(&mut self, pred: Block, inst: Inst) {
        debug_assert!(
            !self.sealed(),
            "sealed blocks cannot accept new predecessors"
        );
        self.predecessors.push(PredBlock::new(pred, inst));
    }

    fn remove_predecessor(&mut self, inst: Inst) -> Block {
        let pred = self
            .predecessors
            .iter()
            .position(|&PredBlock { branch, .. }| branch == inst)
            .expect("the predecessor you are trying to remove is not declared");
        self.predecessors.swap_remove(pred).block
    }

    fn sealed(&self) -> bool {
        matches!(self.sealed, Sealed::Yes)
    }

    fn has_one_predecessor(&self) -> bool {
        self.sealed() && self.predecessors.len() == 1
    }
}

impl SSABuilder {
    /// Clears a `SSABuilder` from all its data, letting it in a pristine state without
    /// deallocating memory.
    pub fn clear(&mut self) {
        self.variables.clear();
        self.ssa_blocks.clear();
        debug_assert!(self.calls.is_empty());
        debug_assert!(self.results.is_empty());
        debug_assert!(self.side_effects.is_empty());
    }

    /// Tests whether an `SSABuilder` is in a cleared state.
    pub fn is_empty(&self) -> bool {
        self.variables.is_empty()
            && self.ssa_blocks.is_empty()
            && self.calls.is_empty()
            && self.results.is_empty()
            && self.side_effects.is_empty()
    }
}

/// States for the `use_var`/`predecessors_lookup` state machine.
enum Call {
    UseVar(Block),
    FinishPredecessorsLookup(Value, Block),
}

/// Emit instructions to produce a zero value in the given type.
fn emit_zero(ty: Type, mut cur: FuncCursor) -> Value {
    if ty.is_int() {
        cur.ins().iconst(ty, 0)
    } else if ty.is_bool() {
        cur.ins().bconst(ty, false)
    } else if ty == F32 {
        cur.ins().f32const(Ieee32::with_bits(0))
    } else if ty == F64 {
        cur.ins().f64const(Ieee64::with_bits(0))
    } else if ty.is_ref() {
        cur.ins().null(ty)
    } else if ty.is_vector() {
        let scalar_ty = ty.lane_type();
        if scalar_ty.is_int() || scalar_ty.is_bool() {
            let zero = cur.func.dfg.constants.insert(
                core::iter::repeat(0)
                    .take(ty.bytes().try_into().unwrap())
                    .collect(),
            );
            cur.ins().vconst(ty, zero)
        } else if scalar_ty == F32 {
            let scalar = cur.ins().f32const(Ieee32::with_bits(0));
            cur.ins().splat(ty, scalar)
        } else if scalar_ty == F64 {
            let scalar = cur.ins().f64const(Ieee64::with_bits(0));
            cur.ins().splat(ty, scalar)
        } else {
            panic!("unimplemented scalar type: {:?}", ty)
        }
    } else {
        panic!("unimplemented type: {:?}", ty)
    }
}

/// The following methods are the API of the SSA builder. Here is how it should be used when
/// translating to Cranelift IR:
///
/// - for each basic block, create a corresponding data for SSA construction with `declare_block`;
///
/// - while traversing a basic block and translating instruction, use `def_var` and `use_var`
///   to record definitions and uses of variables, these methods will give you the corresponding
///   SSA values;
///
/// - when all the instructions in a basic block have translated, the block is said _filled_ and
///   only then you can add it as a predecessor to other blocks with `declare_block_predecessor`;
///
/// - when you have constructed all the predecessor to a basic block,
///   call `seal_block` on it with the `Function` that you are building.
///
/// This API will give you the correct SSA values to use as arguments of your instructions,
/// as well as modify the jump instruction and `Block` parameters to account for the SSA
/// Phi functions.
///
impl SSABuilder {
    /// Declares a new definition of a variable in a given basic block.
    /// The SSA value is passed as an argument because it should be created with
    /// `ir::DataFlowGraph::append_result`.
    pub fn def_var(&mut self, var: Variable, val: Value, block: Block) {
        self.variables[var][block] = PackedOption::from(val);
    }

    /// Declares a use of a variable in a given basic block. Returns the SSA value corresponding
    /// to the current SSA definition of this variable and a list of newly created Blocks that
    /// are the results of critical edge splitting for `br_table` with arguments.
    ///
    /// If the variable has never been defined in this blocks or recursively in its predecessors,
    /// this method will silently create an initializer with `iconst` or `fconst`. You are
    /// responsible for making sure that you initialize your variables.
    pub fn use_var(
        &mut self,
        func: &mut Function,
        var: Variable,
        ty: Type,
        block: Block,
    ) -> (Value, SideEffects) {
        debug_assert!(self.calls.is_empty());
        debug_assert!(self.results.is_empty());
        debug_assert!(self.side_effects.is_empty());

        // Prepare the 'calls' and 'results' stacks for the state machine.
        self.use_var_nonlocal(func, var, ty, block);
        let value = self.run_state_machine(func, var, ty);

        let side_effects = mem::take(&mut self.side_effects);
        (value, side_effects)
    }

    /// Resolve the minimal SSA Value of `var` in `block` by traversing predecessors.
    ///
    /// This function sets up state for `run_state_machine()` but does not execute it.
    fn use_var_nonlocal(&mut self, func: &mut Function, var: Variable, ty: Type, mut block: Block) {
        // First, try Local Value Numbering (Algorithm 1 in the paper).
        // If the variable already has a known Value in this block, use that.
        if let Some(val) = self.variables[var][block].expand() {
            self.results.push(val);
            return;
        }

        // Otherwise, use Global Value Numbering (Algorithm 2 in the paper).
        // This resolves the Value with respect to its predecessors.
        // Find the most recent definition of `var`, and the block the definition comes from.
        let (val, from) = self.find_var(func, var, ty, block);

        // The `from` block returned from `find_var` is guaranteed to be on the path we follow by
        // traversing only single-predecessor edges. It might be equal to `block` if there is no
        // such path, but in that case `find_var` ensures that the variable is defined in this block
        // by a new block parameter. It also might be somewhere in a cycle, but even then this loop
        // will terminate the first time it encounters that block, rather than continuing around the
        // cycle forever.
        //
        // Why is it okay to copy the definition to all intervening blocks? For the initial block,
        // this may not be the final definition of this variable within this block, but if we've
        // gotten here then we know there is no earlier definition in the block already.
        //
        // For the remaining blocks: Recall that a block is only allowed to be set as a predecessor
        // after all its instructions have already been filled in, so when we follow a predecessor
        // edge to a block, we know there will never be any more local variable definitions added to
        // that block. We also know that `find_var` didn't find a definition for this variable in
        // any of the blocks before `from`.
        //
        // So in either case there is no definition in these blocks yet and we can blindly set one.
        let var_defs = &mut self.variables[var];
        while block != from {
            let data = &self.ssa_blocks[block];
            debug_assert!(data.sealed());
            debug_assert_eq!(data.predecessors.len(), 1);
            debug_assert!(var_defs[block].is_none());
            var_defs[block] = PackedOption::from(val);
            block = data.predecessors[0].block;
        }
    }

    /// Find the most recent definition of this variable, returning both the definition and the
    /// block in which it was found. If we can't find a definition that's provably the right one for
    /// all paths to the current block, then append a block parameter to some block and use that as
    /// the definition. Either way, also arrange that the definition will be on the `results` stack
    /// when `run_state_machine` is done processing the current step.
    ///
    /// If a block has exactly one predecessor, and the block is sealed so we know its predecessors
    /// will never change, then its definition for this variable is the same as the definition from
    /// that one predecessor. In this case it's easy to see that no block parameter is necessary,
    /// but we need to look at the predecessor to see if a block parameter might be needed there.
    /// That holds transitively across any chain of sealed blocks with exactly one predecessor each.
    ///
    /// This runs into a problem, though, if such a chain has a cycle: Blindly following a cyclic
    /// chain that never defines this variable would lead to an infinite loop in the compiler. It
    /// doesn't really matter what code we generate in that case. Since each block in the cycle has
    /// exactly one predecessor, there's no way to enter the cycle from the function's entry block;
    /// and since all blocks in the cycle are sealed, the entire cycle is permanently dead code. But
    /// we still have to prevent the possibility of an infinite loop.
    ///
    /// To break cycles, we can pick any block within the cycle as the one where we'll add a block
    /// parameter. It's convenient to pick the block at which we entered the cycle, because that's
    /// the first place where we can detect that we just followed a cycle. Adding a block parameter
    /// gives us a definition we can reuse throughout the rest of the cycle.
    fn find_var(
        &mut self,
        func: &mut Function,
        var: Variable,
        ty: Type,
        mut block: Block,
    ) -> (Value, Block) {
        // Try to find an existing definition along single-predecessor edges first.
        let var_defs = &mut self.variables[var];
        self.visited.clear();
        while self.ssa_blocks[block].has_one_predecessor() && self.visited.insert(block) {
            block = self.ssa_blocks[block].predecessors[0].block;
            if let Some(val) = var_defs[block].expand() {
                self.results.push(val);
                return (val, block);
            }
        }

        // We've promised to return the most recent block where `var` was defined, but we didn't
        // find a usable definition. So create one.
        let val = func.dfg.append_block_param(block, ty);
        var_defs[block] = PackedOption::from(val);

        // Now every predecessor needs to pass its definition of this variable to the newly added
        // block parameter. To do that we have to "recursively" call `use_var`, but there are two
        // problems with doing that. First, we need to keep a fixed bound on stack depth, so we
        // can't actually recurse; instead we defer to `run_state_machine`. Second, if we don't
        // know all our predecessors yet, we have to defer this work until the block gets sealed.
        match &mut self.ssa_blocks[block].sealed {
            // Once all the `calls` added here complete, this leaves either `val` or an equivalent
            // definition on the `results` stack.
            Sealed::Yes => self.begin_predecessors_lookup(val, block),
            Sealed::No { undef_variables } => {
                undef_variables.push((var, val));
                self.results.push(val);
            }
        }
        (val, block)
    }

    /// Declares a new basic block to construct corresponding data for SSA construction.
    /// No predecessors are declared here and the block is not sealed.
    /// Predecessors have to be added with `declare_block_predecessor`.
    pub fn declare_block(&mut self, block: Block) {
        // Ensure the block exists so seal_all_blocks will see it even if no predecessors or
        // variables get declared for this block. But don't assign anything to it:
        // SecondaryMap automatically sets all blocks to `default()`.
        let _ = &mut self.ssa_blocks[block];
    }

    /// Declares a new predecessor for a `Block` and record the branch instruction
    /// of the predecessor that leads to it.
    ///
    /// The precedent `Block` must be filled before added as predecessor.
    /// Note that you must provide no jump arguments to the branch
    /// instruction when you create it since `SSABuilder` will fill them for you.
    ///
    /// Callers are expected to avoid adding the same predecessor more than once in the case
    /// of a jump table.
    pub fn declare_block_predecessor(&mut self, block: Block, pred: Block, inst: Inst) {
        debug_assert!(!self.is_sealed(block));
        self.ssa_blocks[block].add_predecessor(pred, inst)
    }

    /// Remove a previously declared Block predecessor by giving a reference to the jump
    /// instruction. Returns the basic block containing the instruction.
    ///
    /// Note: use only when you know what you are doing, this might break the SSA building problem
    pub fn remove_block_predecessor(&mut self, block: Block, inst: Inst) -> Block {
        debug_assert!(!self.is_sealed(block));
        self.ssa_blocks[block].remove_predecessor(inst)
    }

    /// Completes the global value numbering for a `Block`, all of its predecessors having been
    /// already sealed.
    ///
    /// This method modifies the function's `Layout` by adding arguments to the `Block`s to
    /// take into account the Phi function placed by the SSA algorithm.
    ///
    /// Returns the list of newly created blocks for critical edge splitting.
    pub fn seal_block(&mut self, block: Block, func: &mut Function) -> SideEffects {
        debug_assert!(
            !self.ssa_blocks[block].sealed(),
            "Attempting to seal {} which is already sealed.",
            block
        );
        self.seal_one_block(block, func);
        mem::take(&mut self.side_effects)
    }

    /// Completes the global value numbering for all unsealed `Block`s in `func`.
    ///
    /// It's more efficient to seal `Block`s as soon as possible, during
    /// translation, but for frontends where this is impractical to do, this
    /// function can be used at the end of translating all blocks to ensure
    /// that everything is sealed.
    pub fn seal_all_blocks(&mut self, func: &mut Function) -> SideEffects {
        // Seal all `Block`s currently in the function. This can entail splitting
        // and creation of new blocks, however such new blocks are sealed on
        // the fly, so we don't need to account for them here.
        for block in self.ssa_blocks.keys() {
            self.seal_one_block(block, func);
        }
        mem::take(&mut self.side_effects)
    }

    /// Helper function for `seal_block` and `seal_all_blocks`.
    fn seal_one_block(&mut self, block: Block, func: &mut Function) {
        // For each undef var we look up values in the predecessors and create a block parameter
        // only if necessary.
        for (var, val) in self.mark_block_sealed(block) {
            debug_assert!(self.calls.is_empty());
            debug_assert!(self.results.is_empty());
            // self.side_effects may be non-empty here so that callers can
            // accumulate side effects over multiple calls.
            self.begin_predecessors_lookup(val, block);
            self.run_state_machine(func, var, func.dfg.value_type(val));
        }
    }

    /// Set the `sealed` flag for `block`. Returns any variables that still need definitions.
    fn mark_block_sealed(&mut self, block: Block) -> Vec<(Variable, Value)> {
        // We could call data.predecessors.shrink_to_fit() here, if
        // important, because no further predecessors will be added
        // to this block.

        match mem::replace(&mut self.ssa_blocks[block].sealed, Sealed::Yes) {
            Sealed::No { undef_variables } => undef_variables,
            Sealed::Yes => Vec::new(),
        }
    }

    /// Given the local SSA Value of a Variable in a Block, perform a recursive lookup on
    /// predecessors to determine if it is redundant with another Value earlier in the CFG.
    ///
    /// If such a Value exists and is redundant, the local Value is replaced by the
    /// corresponding non-local Value. If the original Value was a Block parameter,
    /// the parameter may be removed if redundant. Parameters are placed eagerly by callers
    /// to avoid infinite loops when looking up a Value for a Block that is in a CFG loop.
    ///
    /// Doing this lookup for each Value in each Block preserves SSA form during construction.
    ///
    /// ## Arguments
    ///
    /// `sentinel` is a dummy Block parameter inserted by `use_var_nonlocal()`.
    /// Its purpose is to allow detection of CFG cycles while traversing predecessors.
    fn begin_predecessors_lookup(&mut self, sentinel: Value, dest_block: Block) {
        self.calls
            .push(Call::FinishPredecessorsLookup(sentinel, dest_block));
        // Iterate over the predecessors.
        self.calls.extend(
            self.ssa_blocks[dest_block]
                .predecessors
                .iter()
                .rev()
                .map(|pred| Call::UseVar(pred.block)),
        );
    }

    /// Examine the values from the predecessors and compute a result value, creating
    /// block parameters as needed.
    fn finish_predecessors_lookup(
        &mut self,
        func: &mut Function,
        sentinel: Value,
        var: Variable,
        dest_block: Block,
    ) {
        // Determine how many predecessors are yielding unique, non-temporary Values. If a variable
        // is live and unmodified across several control-flow join points, earlier blocks will
        // introduce aliases for that variable's definition, so we resolve aliases eagerly here to
        // ensure that we can tell when the same definition has reached this block via multiple
        // paths. Doing so also detects cyclic references to the sentinel, which can occur in
        // unreachable code.
        let pred_val = {
            let num_predecessors = self.predecessors(dest_block).len();
            let mut iter = self
                .results
                .drain(self.results.len() - num_predecessors..)
                .map(|val| func.dfg.resolve_aliases(val))
                .filter(|&val| val != sentinel);
            if let Some(val) = iter.next() {
                // This variable has at least one non-temporary definition. If they're all the same
                // value, we can remove the block parameter and reference that value instead.
                if iter.all(|other| other == val) {
                    Some(val)
                } else {
                    None
                }
            } else {
                // The variable is used but never defined before. This is an irregularity in the
                // code, but rather than throwing an error we silently initialize the variable to
                // 0. This will have no effect since this situation happens in unreachable code.
                if !func.layout.is_block_inserted(dest_block) {
                    func.layout.append_block(dest_block);
                }
                self.side_effects
                    .instructions_added_to_blocks
                    .push(dest_block);
                let zero = emit_zero(
                    func.dfg.value_type(sentinel),
                    FuncCursor::new(func).at_first_insertion_point(dest_block),
                );
                Some(zero)
            }
        };

        let result_val = if let Some(pred_val) = pred_val {
            // Here all the predecessors use a single value to represent our variable
            // so we don't need to have it as a block argument.
            // We need to replace all the occurrences of val with pred_val but since
            // we can't afford a re-writing pass right now we just declare an alias.
            func.dfg.remove_block_param(sentinel);
            func.dfg.change_to_alias(sentinel, pred_val);
            pred_val
        } else {
            // There is disagreement in the predecessors on which value to use so we have
            // to keep the block argument. To avoid borrowing `self` for the whole loop,
            // temporarily detach the predecessors list and replace it with an empty list.
            let mut preds = mem::take(self.predecessors_mut(dest_block));
            for pred in preds.iter_mut() {
                self.append_jump_argument(func, pred, dest_block, var);
            }
            // Now that we're done, move the predecessors list back.
            debug_assert!(self.predecessors(dest_block).is_empty());
            *self.predecessors_mut(dest_block) = preds;
            sentinel
        };

        self.results.push(result_val);
    }

    /// Appends a jump argument to a jump instruction, returns block created in case of
    /// critical edge splitting.
    fn append_jump_argument(
        &mut self,
        func: &mut Function,
        pred: &mut PredBlock,
        dest_block: Block,
        var: Variable,
    ) {
        // We already did a full `use_var` above, so we can do just the fast path.
        let val = self.variables[var][pred.block].unwrap();
        match func.dfg.analyze_branch(pred.branch) {
            BranchInfo::NotABranch => {
                panic!("you have declared a non-branch instruction as a predecessor to a block");
            }
            // For a single destination appending a jump argument to the instruction
            // is sufficient.
            BranchInfo::SingleDest(_, _) => func.dfg.append_inst_arg(pred.branch, val),
            BranchInfo::Table(mut jt, _default_block) => {
                // In the case of a jump table, the situation is tricky because br_table doesn't
                // support arguments. We have to split the critical edge.
                let middle_block = func.dfg.make_block();
                func.layout.append_block(middle_block);
                self.declare_block(middle_block);
                self.ssa_blocks[middle_block].add_predecessor(pred.block, pred.branch);
                self.mark_block_sealed(middle_block);

                let table = &func.jump_tables[jt];
                let mut copied = JumpTableData::with_capacity(table.len());
                let mut changed = false;
                for &destination in table.iter() {
                    if destination == dest_block {
                        copied.push_entry(middle_block);
                        changed = true;
                    } else {
                        copied.push_entry(destination);
                    }
                }

                if changed {
                    jt = func.create_jump_table(copied);
                }

                // Redo the match from `analyze_branch` but this time capture mutable references
                match &mut func.dfg[pred.branch] {
                    InstructionData::BranchTable {
                        destination, table, ..
                    } => {
                        if *destination == dest_block {
                            *destination = middle_block;
                        }
                        *table = jt;
                    }
                    _ => unreachable!(),
                }

                let mut cur = FuncCursor::new(func).at_bottom(middle_block);
                let middle_jump_inst = cur.ins().jump(dest_block, &[val]);
                self.def_var(var, val, middle_block);
                pred.block = middle_block;
                pred.branch = middle_jump_inst;
                self.side_effects.split_blocks_created.push(middle_block);
            }
        }
    }

    /// Returns the list of `Block`s that have been declared as predecessors of the argument.
    fn predecessors(&self, block: Block) -> &[PredBlock] {
        &self.ssa_blocks[block].predecessors
    }

    /// Returns whether the given Block has any predecessor or not.
    pub fn has_any_predecessors(&self, block: Block) -> bool {
        !self.predecessors(block).is_empty()
    }

    /// Same as predecessors, but for &mut.
    fn predecessors_mut(&mut self, block: Block) -> &mut PredBlockSmallVec {
        &mut self.ssa_blocks[block].predecessors
    }

    /// Returns `true` if and only if `seal_block` has been called on the argument.
    pub fn is_sealed(&self, block: Block) -> bool {
        self.ssa_blocks[block].sealed()
    }

    /// The main algorithm is naturally recursive: when there's a `use_var` in a
    /// block with no corresponding local defs, it recurses and performs a
    /// `use_var` in each predecessor. To avoid risking running out of callstack
    /// space, we keep an explicit stack and use a small state machine rather
    /// than literal recursion.
    fn run_state_machine(&mut self, func: &mut Function, var: Variable, ty: Type) -> Value {
        // Process the calls scheduled in `self.calls` until it is empty.
        while let Some(call) = self.calls.pop() {
            match call {
                Call::UseVar(block) => self.use_var_nonlocal(func, var, ty, block),
                Call::FinishPredecessorsLookup(sentinel, dest_block) => {
                    self.finish_predecessors_lookup(func, sentinel, var, dest_block);
                }
            }
        }
        debug_assert_eq!(self.results.len(), 1);
        self.results.pop().unwrap()
    }
}

#[cfg(test)]
mod tests {
    use crate::ssa::SSABuilder;
    use crate::Variable;
    use cranelift_codegen::cursor::{Cursor, FuncCursor};
    use cranelift_codegen::entity::EntityRef;
    use cranelift_codegen::ir::instructions::BranchInfo;
    use cranelift_codegen::ir::types::*;
    use cranelift_codegen::ir::{Function, Inst, InstBuilder, JumpTableData, Opcode};
    use cranelift_codegen::settings;
    use cranelift_codegen::verify_function;

    #[test]
    fn simple_block() {
        let mut func = Function::new();
        let mut ssa = SSABuilder::default();
        let block0 = func.dfg.make_block();
        // Here is the pseudo-program we want to translate:
        // block0:
        //    x = 1;
        //    y = 2;
        //    z = x + y;
        //    z = x + z;

        ssa.declare_block(block0);
        let x_var = Variable::new(0);
        let x_ssa = {
            let mut cur = FuncCursor::new(&mut func);
            cur.insert_block(block0);
            cur.ins().iconst(I32, 1)
        };
        ssa.def_var(x_var, x_ssa, block0);
        let y_var = Variable::new(1);
        let y_ssa = {
            let mut cur = FuncCursor::new(&mut func).at_bottom(block0);
            cur.ins().iconst(I32, 2)
        };
        ssa.def_var(y_var, y_ssa, block0);
        assert_eq!(ssa.use_var(&mut func, x_var, I32, block0).0, x_ssa);
        assert_eq!(ssa.use_var(&mut func, y_var, I32, block0).0, y_ssa);

        let z_var = Variable::new(2);
        let x_use1 = ssa.use_var(&mut func, x_var, I32, block0).0;
        let y_use1 = ssa.use_var(&mut func, y_var, I32, block0).0;
        let z1_ssa = {
            let mut cur = FuncCursor::new(&mut func).at_bottom(block0);
            cur.ins().iadd(x_use1, y_use1)
        };
        ssa.def_var(z_var, z1_ssa, block0);
        assert_eq!(ssa.use_var(&mut func, z_var, I32, block0).0, z1_ssa);

        let x_use2 = ssa.use_var(&mut func, x_var, I32, block0).0;
        let z_use1 = ssa.use_var(&mut func, z_var, I32, block0).0;
        let z2_ssa = {
            let mut cur = FuncCursor::new(&mut func).at_bottom(block0);
            cur.ins().iadd(x_use2, z_use1)
        };
        ssa.def_var(z_var, z2_ssa, block0);
        assert_eq!(ssa.use_var(&mut func, z_var, I32, block0).0, z2_ssa);
    }

    #[test]
    fn sequence_of_blocks() {
        let mut func = Function::new();
        let mut ssa = SSABuilder::default();
        let block0 = func.dfg.make_block();
        let block1 = func.dfg.make_block();
        let block2 = func.dfg.make_block();
        // Here is the pseudo-program we want to translate:
        // block0:
        //    x = 1;
        //    y = 2;
        //    z = x + y;
        //    brnz y, block1;
        //    jump block1;
        // block1:
        //    z = x + z;
        //    jump block2;
        // block2:
        //    y = x + y;
        {
            let mut cur = FuncCursor::new(&mut func);
            cur.insert_block(block0);
            cur.insert_block(block1);
            cur.insert_block(block2);
        }

        // block0
        ssa.declare_block(block0);
        ssa.seal_block(block0, &mut func);
        let x_var = Variable::new(0);
        let x_ssa = {
            let mut cur = FuncCursor::new(&mut func).at_bottom(block0);
            cur.ins().iconst(I32, 1)
        };
        ssa.def_var(x_var, x_ssa, block0);
        let y_var = Variable::new(1);
        let y_ssa = {
            let mut cur = FuncCursor::new(&mut func).at_bottom(block0);
            cur.ins().iconst(I32, 2)
        };
        ssa.def_var(y_var, y_ssa, block0);
        let z_var = Variable::new(2);
        let x_use1 = ssa.use_var(&mut func, x_var, I32, block0).0;
        let y_use1 = ssa.use_var(&mut func, y_var, I32, block0).0;
        let z1_ssa = {
            let mut cur = FuncCursor::new(&mut func).at_bottom(block0);
            cur.ins().iadd(x_use1, y_use1)
        };
        ssa.def_var(z_var, z1_ssa, block0);
        let y_use2 = ssa.use_var(&mut func, y_var, I32, block0).0;
        let brnz_block0_block2: Inst = {
            let mut cur = FuncCursor::new(&mut func).at_bottom(block0);
            cur.ins().brnz(y_use2, block2, &[])
        };
        let jump_block0_block1: Inst = {
            let mut cur = FuncCursor::new(&mut func).at_bottom(block0);
            cur.ins().jump(block1, &[])
        };

        assert_eq!(ssa.use_var(&mut func, x_var, I32, block0).0, x_ssa);
        assert_eq!(ssa.use_var(&mut func, y_var, I32, block0).0, y_ssa);
        assert_eq!(ssa.use_var(&mut func, z_var, I32, block0).0, z1_ssa);

        // block1
        ssa.declare_block(block1);
        ssa.declare_block_predecessor(block1, block0, jump_block0_block1);
        ssa.seal_block(block1, &mut func);

        let x_use2 = ssa.use_var(&mut func, x_var, I32, block1).0;
        let z_use1 = ssa.use_var(&mut func, z_var, I32, block1).0;
        let z2_ssa = {
            let mut cur = FuncCursor::new(&mut func).at_bottom(block1);
            cur.ins().iadd(x_use2, z_use1)
        };
        ssa.def_var(z_var, z2_ssa, block1);
        let jump_block1_block2: Inst = {
            let mut cur = FuncCursor::new(&mut func).at_bottom(block1);
            cur.ins().jump(block2, &[])
        };

        assert_eq!(x_use2, x_ssa);
        assert_eq!(z_use1, z1_ssa);
        assert_eq!(ssa.use_var(&mut func, z_var, I32, block1).0, z2_ssa);

        // block2
        ssa.declare_block(block2);
        ssa.declare_block_predecessor(block2, block0, brnz_block0_block2);
        ssa.declare_block_predecessor(block2, block1, jump_block1_block2);
        ssa.seal_block(block2, &mut func);
        let x_use3 = ssa.use_var(&mut func, x_var, I32, block2).0;
        let y_use3 = ssa.use_var(&mut func, y_var, I32, block2).0;
        let y2_ssa = {
            let mut cur = FuncCursor::new(&mut func).at_bottom(block2);
            cur.ins().iadd(x_use3, y_use3)
        };
        ssa.def_var(y_var, y2_ssa, block2);

        assert_eq!(x_ssa, x_use3);
        assert_eq!(y_ssa, y_use3);
        match func.dfg.analyze_branch(brnz_block0_block2) {
            BranchInfo::SingleDest(dest, jump_args) => {
                assert_eq!(dest, block2);
                assert_eq!(jump_args.len(), 0);
            }
            _ => assert!(false),
        };
        match func.dfg.analyze_branch(jump_block0_block1) {
            BranchInfo::SingleDest(dest, jump_args) => {
                assert_eq!(dest, block1);
                assert_eq!(jump_args.len(), 0);
            }
            _ => assert!(false),
        };
        match func.dfg.analyze_branch(jump_block1_block2) {
            BranchInfo::SingleDest(dest, jump_args) => {
                assert_eq!(dest, block2);
                assert_eq!(jump_args.len(), 0);
            }
            _ => assert!(false),
        };
    }

    #[test]
    fn program_with_loop() {
        let mut func = Function::new();
        let mut ssa = SSABuilder::default();
        let block0 = func.dfg.make_block();
        let block1 = func.dfg.make_block();
        let block2 = func.dfg.make_block();
        let block3 = func.dfg.make_block();
        {
            let mut cur = FuncCursor::new(&mut func);
            cur.insert_block(block0);
            cur.insert_block(block1);
            cur.insert_block(block2);
            cur.insert_block(block3);
        }
        // Here is the pseudo-program we want to translate:
        // block0:
        //    x = 1;
        //    y = 2;
        //    z = x + y;
        //    jump block1
        // block1:
        //    z = z + y;
        //    brnz y, block3;
        //    jump block2;
        // block2:
        //    z = z - x;
        //    return y
        // block3:
        //    y = y - x
        //    jump block1

        // block0
        ssa.declare_block(block0);
        ssa.seal_block(block0, &mut func);
        let x_var = Variable::new(0);
        let x1 = {
            let mut cur = FuncCursor::new(&mut func).at_bottom(block0);
            cur.ins().iconst(I32, 1)
        };
        ssa.def_var(x_var, x1, block0);
        let y_var = Variable::new(1);
        let y1 = {
            let mut cur = FuncCursor::new(&mut func).at_bottom(block0);
            cur.ins().iconst(I32, 2)
        };
        ssa.def_var(y_var, y1, block0);
        let z_var = Variable::new(2);
        let x2 = ssa.use_var(&mut func, x_var, I32, block0).0;
        let y2 = ssa.use_var(&mut func, y_var, I32, block0).0;
        let z1 = {
            let mut cur = FuncCursor::new(&mut func).at_bottom(block0);
            cur.ins().iadd(x2, y2)
        };
        ssa.def_var(z_var, z1, block0);
        let jump_block0_block1 = {
            let mut cur = FuncCursor::new(&mut func).at_bottom(block0);
            cur.ins().jump(block1, &[])
        };
        assert_eq!(ssa.use_var(&mut func, x_var, I32, block0).0, x1);
        assert_eq!(ssa.use_var(&mut func, y_var, I32, block0).0, y1);
        assert_eq!(x2, x1);
        assert_eq!(y2, y1);

        // block1
        ssa.declare_block(block1);
        ssa.declare_block_predecessor(block1, block0, jump_block0_block1);
        let z2 = ssa.use_var(&mut func, z_var, I32, block1).0;
        let y3 = ssa.use_var(&mut func, y_var, I32, block1).0;
        let z3 = {
            let mut cur = FuncCursor::new(&mut func).at_bottom(block1);
            cur.ins().iadd(z2, y3)
        };
        ssa.def_var(z_var, z3, block1);
        let y4 = ssa.use_var(&mut func, y_var, I32, block1).0;
        assert_eq!(y4, y3);
        let brnz_block1_block3 = {
            let mut cur = FuncCursor::new(&mut func).at_bottom(block1);
            cur.ins().brnz(y4, block3, &[])
        };
        let jump_block1_block2 = {
            let mut cur = FuncCursor::new(&mut func).at_bottom(block1);
            cur.ins().jump(block2, &[])
        };

        // block2
        ssa.declare_block(block2);
        ssa.declare_block_predecessor(block2, block1, jump_block1_block2);
        ssa.seal_block(block2, &mut func);
        let z4 = ssa.use_var(&mut func, z_var, I32, block2).0;
        assert_eq!(z4, z3);
        let x3 = ssa.use_var(&mut func, x_var, I32, block2).0;
        let z5 = {
            let mut cur = FuncCursor::new(&mut func).at_bottom(block2);
            cur.ins().isub(z4, x3)
        };
        ssa.def_var(z_var, z5, block2);
        let y5 = ssa.use_var(&mut func, y_var, I32, block2).0;
        assert_eq!(y5, y3);
        {
            let mut cur = FuncCursor::new(&mut func).at_bottom(block2);
            cur.ins().return_(&[y5])
        };

        // block3
        ssa.declare_block(block3);
        ssa.declare_block_predecessor(block3, block1, brnz_block1_block3);
        ssa.seal_block(block3, &mut func);
        let y6 = ssa.use_var(&mut func, y_var, I32, block3).0;
        assert_eq!(y6, y3);
        let x4 = ssa.use_var(&mut func, x_var, I32, block3).0;
        assert_eq!(x4, x3);
        let y7 = {
            let mut cur = FuncCursor::new(&mut func).at_bottom(block3);
            cur.ins().isub(y6, x4)
        };
        ssa.def_var(y_var, y7, block3);
        let jump_block3_block1 = {
            let mut cur = FuncCursor::new(&mut func).at_bottom(block3);
            cur.ins().jump(block1, &[])
        };

        // block1 after all predecessors have been visited.
        ssa.declare_block_predecessor(block1, block3, jump_block3_block1);
        ssa.seal_block(block1, &mut func);
        assert_eq!(func.dfg.block_params(block1)[0], z2);
        assert_eq!(func.dfg.block_params(block1)[1], y3);
        assert_eq!(func.dfg.resolve_aliases(x3), x1);
    }

    #[test]
    fn br_table_with_args() {
        // This tests the on-demand splitting of critical edges for br_table with jump arguments
        //
        // Here is the pseudo-program we want to translate:
        //
        // function %f {
        // jt = jump_table [block2, block1]
        // block0:
        //    x = 1;
        //    br_table x, block2, jt
        // block1:
        //    x = 2
        //    jump block2
        // block2:
        //    x = x + 1
        //    return
        // }

        let mut func = Function::new();
        let mut ssa = SSABuilder::default();
        let block0 = func.dfg.make_block();
        let block1 = func.dfg.make_block();
        let block2 = func.dfg.make_block();
        let mut jump_table = JumpTableData::new();
        jump_table.push_entry(block2);
        jump_table.push_entry(block1);
        {
            let mut cur = FuncCursor::new(&mut func);
            cur.insert_block(block0);
            cur.insert_block(block1);
            cur.insert_block(block2);
        }

        // block0
        let x1 = {
            let mut cur = FuncCursor::new(&mut func).at_bottom(block0);
            cur.ins().iconst(I32, 1)
        };
        ssa.declare_block(block0);
        ssa.seal_block(block0, &mut func);
        let x_var = Variable::new(0);
        ssa.def_var(x_var, x1, block0);
        ssa.use_var(&mut func, x_var, I32, block0).0;
        let br_table = {
            let jt = func.create_jump_table(jump_table);
            let mut cur = FuncCursor::new(&mut func).at_bottom(block0);
            cur.ins().br_table(x1, block2, jt)
        };

        // block1
        ssa.declare_block(block1);
        ssa.declare_block_predecessor(block1, block0, br_table);
        ssa.seal_block(block1, &mut func);
        let x2 = {
            let mut cur = FuncCursor::new(&mut func).at_bottom(block1);
            cur.ins().iconst(I32, 2)
        };
        ssa.def_var(x_var, x2, block1);
        let jump_block1_block2 = {
            let mut cur = FuncCursor::new(&mut func).at_bottom(block1);
            cur.ins().jump(block2, &[])
        };

        // block2
        ssa.declare_block(block2);
        ssa.declare_block_predecessor(block2, block1, jump_block1_block2);
        ssa.declare_block_predecessor(block2, block0, br_table);
        ssa.seal_block(block2, &mut func);
        let x3 = ssa.use_var(&mut func, x_var, I32, block2).0;
        let x4 = {
            let mut cur = FuncCursor::new(&mut func).at_bottom(block2);
            cur.ins().iadd_imm(x3, 1)
        };
        ssa.def_var(x_var, x4, block2);
        {
            let mut cur = FuncCursor::new(&mut func).at_bottom(block2);
            cur.ins().return_(&[])
        };

        let flags = settings::Flags::new(settings::builder());
        match verify_function(&func, &flags) {
            Ok(()) => {}
            Err(_errors) => {
                #[cfg(feature = "std")]
                panic!("{}", _errors);
                #[cfg(not(feature = "std"))]
                panic!("function failed to verify");
            }
        }
    }

    #[test]
    fn undef_values_reordering() {
        // Here is the pseudo-program we want to translate:
        // block0:
        //    x = 0;
        //    y = 1;
        //    z = 2;
        //    jump block1;
        // block1:
        //    x = z + x;
        //    y = y - x;
        //    jump block1;
        //
        let mut func = Function::new();
        let mut ssa = SSABuilder::default();
        let block0 = func.dfg.make_block();
        let block1 = func.dfg.make_block();
        {
            let mut cur = FuncCursor::new(&mut func);
            cur.insert_block(block0);
            cur.insert_block(block1);
        }

        // block0
        ssa.declare_block(block0);
        let x_var = Variable::new(0);
        ssa.seal_block(block0, &mut func);
        let x1 = {
            let mut cur = FuncCursor::new(&mut func).at_bottom(block0);
            cur.ins().iconst(I32, 0)
        };
        ssa.def_var(x_var, x1, block0);
        let y_var = Variable::new(1);
        let y1 = {
            let mut cur = FuncCursor::new(&mut func).at_bottom(block0);
            cur.ins().iconst(I32, 1)
        };
        ssa.def_var(y_var, y1, block0);
        let z_var = Variable::new(2);
        let z1 = {
            let mut cur = FuncCursor::new(&mut func).at_bottom(block0);
            cur.ins().iconst(I32, 2)
        };
        ssa.def_var(z_var, z1, block0);
        let jump_block0_block1 = {
            let mut cur = FuncCursor::new(&mut func).at_bottom(block0);
            cur.ins().jump(block1, &[])
        };

        // block1
        ssa.declare_block(block1);
        ssa.declare_block_predecessor(block1, block0, jump_block0_block1);
        let z2 = ssa.use_var(&mut func, z_var, I32, block1).0;
        assert_eq!(func.dfg.block_params(block1)[0], z2);
        let x2 = ssa.use_var(&mut func, x_var, I32, block1).0;
        assert_eq!(func.dfg.block_params(block1)[1], x2);
        let x3 = {
            let mut cur = FuncCursor::new(&mut func).at_bottom(block1);
            cur.ins().iadd(x2, z2)
        };
        ssa.def_var(x_var, x3, block1);
        let x4 = ssa.use_var(&mut func, x_var, I32, block1).0;
        let y3 = ssa.use_var(&mut func, y_var, I32, block1).0;
        assert_eq!(func.dfg.block_params(block1)[2], y3);
        let y4 = {
            let mut cur = FuncCursor::new(&mut func).at_bottom(block1);
            cur.ins().isub(y3, x4)
        };
        ssa.def_var(y_var, y4, block1);
        let jump_block1_block1 = {
            let mut cur = FuncCursor::new(&mut func).at_bottom(block1);
            cur.ins().jump(block1, &[])
        };
        ssa.declare_block_predecessor(block1, block1, jump_block1_block1);
        ssa.seal_block(block1, &mut func);
        // At sealing the "z" argument disappear but the remaining "x" and "y" args have to be
        // in the right order.
        assert_eq!(func.dfg.block_params(block1)[1], y3);
        assert_eq!(func.dfg.block_params(block1)[0], x2);
    }

    #[test]
    fn undef() {
        // Use vars of various types which have not been defined.
        let mut func = Function::new();
        let mut ssa = SSABuilder::default();
        let block0 = func.dfg.make_block();
        ssa.declare_block(block0);
        ssa.seal_block(block0, &mut func);
        let i32_var = Variable::new(0);
        let f32_var = Variable::new(1);
        let f64_var = Variable::new(2);
        let b1_var = Variable::new(3);
        let f32x4_var = Variable::new(4);
        ssa.use_var(&mut func, i32_var, I32, block0);
        ssa.use_var(&mut func, f32_var, F32, block0);
        ssa.use_var(&mut func, f64_var, F64, block0);
        ssa.use_var(&mut func, b1_var, B1, block0);
        ssa.use_var(&mut func, f32x4_var, F32X4, block0);
        assert_eq!(func.dfg.num_block_params(block0), 0);
    }

    #[test]
    fn undef_in_entry() {
        // Use a var which has not been defined. The search should hit the
        // top of the entry block, and then fall back to inserting an iconst.
        let mut func = Function::new();
        let mut ssa = SSABuilder::default();
        let block0 = func.dfg.make_block();
        ssa.declare_block(block0);
        ssa.seal_block(block0, &mut func);
        let x_var = Variable::new(0);
        assert_eq!(func.dfg.num_block_params(block0), 0);
        ssa.use_var(&mut func, x_var, I32, block0);
        assert_eq!(func.dfg.num_block_params(block0), 0);
        assert_eq!(
            func.dfg[func.layout.first_inst(block0).unwrap()].opcode(),
            Opcode::Iconst
        );
    }

    #[test]
    fn undef_in_entry_sealed_after() {
        // Use a var which has not been defined, but the block is not sealed
        // until afterward. Before sealing, the SSA builder should insert an
        // block param; after sealing, it should be removed.
        let mut func = Function::new();
        let mut ssa = SSABuilder::default();
        let block0 = func.dfg.make_block();
        ssa.declare_block(block0);
        let x_var = Variable::new(0);
        assert_eq!(func.dfg.num_block_params(block0), 0);
        ssa.use_var(&mut func, x_var, I32, block0);
        assert_eq!(func.dfg.num_block_params(block0), 1);
        ssa.seal_block(block0, &mut func);
        assert_eq!(func.dfg.num_block_params(block0), 0);
        assert_eq!(
            func.dfg[func.layout.first_inst(block0).unwrap()].opcode(),
            Opcode::Iconst
        );
    }

    #[test]
    fn unreachable_use() {
        // Here is the pseudo-program we want to translate:
        // block0:
        //    return;
        // block1:
        //    brz x, block1;
        //    jump block1;
        let mut func = Function::new();
        let mut ssa = SSABuilder::default();
        let block0 = func.dfg.make_block();
        let block1 = func.dfg.make_block();
        {
            let mut cur = FuncCursor::new(&mut func);
            cur.insert_block(block0);
            cur.insert_block(block1);
        }

        // block0
        ssa.declare_block(block0);
        ssa.seal_block(block0, &mut func);
        {
            let mut cur = FuncCursor::new(&mut func).at_bottom(block0);
            cur.ins().return_(&[]);
        }

        // block1
        ssa.declare_block(block1);
        {
            let mut cur = FuncCursor::new(&mut func).at_bottom(block1);
            let x_var = Variable::new(0);
            let x_val = ssa.use_var(&mut cur.func, x_var, I32, block1).0;
            let brz = cur.ins().brz(x_val, block1, &[]);
            let jump_block1_block1 = cur.ins().jump(block1, &[]);
            ssa.declare_block_predecessor(block1, block1, brz);
            ssa.declare_block_predecessor(block1, block1, jump_block1_block1);
        }
        ssa.seal_block(block1, &mut func);

        let flags = settings::Flags::new(settings::builder());
        match verify_function(&func, &flags) {
            Ok(()) => {}
            Err(_errors) => {
                #[cfg(feature = "std")]
                panic!("{}", _errors);
                #[cfg(not(feature = "std"))]
                panic!("function failed to verify");
            }
        }
    }

    #[test]
    fn unreachable_use_with_multiple_preds() {
        // Here is the pseudo-program we want to translate:
        // block0:
        //    return;
        // block1:
        //    brz x, block2;
        //    jump block1;
        // block2:
        //    jump block1;
        let mut func = Function::new();
        let mut ssa = SSABuilder::default();
        let block0 = func.dfg.make_block();
        let block1 = func.dfg.make_block();
        let block2 = func.dfg.make_block();
        {
            let mut cur = FuncCursor::new(&mut func);
            cur.insert_block(block0);
            cur.insert_block(block1);
            cur.insert_block(block2);
        }

        // block0
        ssa.declare_block(block0);
        ssa.seal_block(block0, &mut func);
        {
            let mut cur = FuncCursor::new(&mut func).at_bottom(block0);
            cur.ins().return_(&[]);
        }

        // block1
        ssa.declare_block(block1);
        let brz = {
            let mut cur = FuncCursor::new(&mut func).at_bottom(block1);
            let x_var = Variable::new(0);
            let x_val = ssa.use_var(&mut cur.func, x_var, I32, block1).0;
            let brz = cur.ins().brz(x_val, block2, &[]);
            let jump_block1_block1 = cur.ins().jump(block1, &[]);
            ssa.declare_block_predecessor(block1, block1, jump_block1_block1);
            brz
        };

        // block2
        ssa.declare_block(block2);
        ssa.declare_block_predecessor(block2, block1, brz);
        ssa.seal_block(block2, &mut func);
        let jump_block2_block1 = {
            let mut cur = FuncCursor::new(&mut func).at_bottom(block2);
            cur.ins().jump(block1, &[])
        };

        // seal block1
        ssa.declare_block_predecessor(block1, block2, jump_block2_block1);
        ssa.seal_block(block1, &mut func);
        let flags = settings::Flags::new(settings::builder());
        match verify_function(&func, &flags) {
            Ok(()) => {}
            Err(_errors) => {
                #[cfg(feature = "std")]
                panic!("{}", _errors);
                #[cfg(not(feature = "std"))]
                panic!("function failed to verify");
            }
        }
    }

    #[test]
    fn reassign_with_predecessor_loop_hangs() {
        // Here is the pseudo-program we want to translate:
        // block0:
        //    var0 = iconst 0
        //    return;
        // block1:
        //    jump block2;
        // block2:
        //    ; phantom use of var0
        //    var0 = iconst 1
        //    jump block1;

        let mut func = Function::new();
        let mut ssa = SSABuilder::default();
        let block0 = func.dfg.make_block();
        let block1 = func.dfg.make_block();
        let block2 = func.dfg.make_block();
        let var0 = Variable::new(0);

        {
            let mut cur = FuncCursor::new(&mut func);
            for block in [block0, block1, block2] {
                cur.insert_block(block);
                ssa.declare_block(block);
            }
        }

        // block0
        {
            let mut cur = FuncCursor::new(&mut func).at_bottom(block0);

            let var0_iconst = cur.ins().iconst(I32, 0);
            ssa.def_var(var0, var0_iconst, block0);

            cur.ins().return_(&[]);
        }

        // block1
        {
            let mut cur = FuncCursor::new(&mut func).at_bottom(block1);

            let jump = cur.ins().jump(block2, &[]);
            ssa.declare_block_predecessor(block2, block1, jump);
        }

        // block2
        {
            let mut cur = FuncCursor::new(&mut func).at_bottom(block2);

            let _ = ssa.use_var(&mut cur.func, var0, I32, block2).0;
            let var0_iconst = cur.ins().iconst(I32, 1);
            ssa.def_var(var0, var0_iconst, block2);

            let jump = cur.ins().jump(block1, &[]);
            ssa.declare_block_predecessor(block1, block1, jump);
        }

        // The sealing algorithm would enter a infinite loop here
        ssa.seal_all_blocks(&mut func);
    }
}
