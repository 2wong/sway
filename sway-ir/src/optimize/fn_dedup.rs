//! ## Deduplicate functions.
//!
//! If two functions are functionally identical, eliminate one
//! and replace all calls to it with a call to the retained one.
//!
//! This pass shouldn't be required once the monomorphiser stops
//! generating a new function for each instantiation even when the exact
//! same instantiation exists.

use std::hash::{Hash, Hasher};

use rustc_hash::{FxHashMap, FxHashSet, FxHasher};

use crate::{
    build_call_graph, callee_first_order, AnalysisResults, Block, Context, Function, InstOp,
    Instruction, IrError, Module, Pass, PassMutability, ScopedPass, Value,
};

pub const FNDEDUP_NAME: &str = "fndedup";

pub fn create_fn_dedup_pass() -> Pass {
    Pass {
        name: FNDEDUP_NAME,
        descr: "Deduplicate functions.",
        deps: vec![],
        runner: ScopedPass::ModulePass(PassMutability::Transform(dedup_fns)),
    }
}

// Functions that are equivalent are put in the same set.
struct EqClass {
    // Map a function hash to its equivalence class.
    hash_set_map: FxHashMap<u64, FxHashSet<Function>>,
    // Once we compute the hash of a function, it's noted here.
    function_hash_map: FxHashMap<Function, u64>,
}

fn hash_fn(context: &Context, function: Function, eq_class: &mut EqClass) -> u64 {
    let state = &mut FxHasher::default();

    // A unique, but only in this function, ID for values.
    let localised_value_id: &mut FxHashMap<Value, u64> = &mut FxHashMap::default();
    // A unique, but only in this function, ID for blocks.
    let localised_block_id: &mut FxHashMap<Block, u64> = &mut FxHashMap::default();
    // TODO: We could do a similar localised ID'ing of local variable names
    // and ASM block arguments too, thereby slightly relaxing the equality check.

    fn get_localised_id<T: Eq + Hash>(t: T, map: &mut FxHashMap<T, u64>) -> u64 {
        let cur_count = map.len();
        *map.entry(t).or_insert(cur_count as u64)
    }

    fn hash_value(
        context: &Context,
        v: Value,
        localised_value_id: &mut FxHashMap<Value, u64>,
        hasher: &mut FxHasher,
    ) {
        match &context.values.get(v.0).unwrap().value {
            crate::ValueDatum::Argument(_) | crate::ValueDatum::Instruction(_) => {
                get_localised_id(v, localised_value_id).hash(hasher)
            }
            crate::ValueDatum::Configurable(c) | crate::ValueDatum::Constant(c) => c.hash(hasher),
        }
    }

    // Start with the function return type.
    function.get_return_type(context).hash(state);

    // ... and local variables.
    for (local_name, local_var) in function.locals_iter(context) {
        local_name.hash(state);
        if let Some(init) = local_var.get_initializer(context) {
            init.hash(state);
        }
        local_var.get_type(context).hash(state);
    }

    // Process every block, first its arguments and then the instructions.
    for block in function.block_iter(context) {
        get_localised_id(block, localised_block_id).hash(state);
        for &arg in block.arg_iter(context) {
            get_localised_id(arg, localised_value_id).hash(state);
            arg.get_argument(context).unwrap().ty.hash(state);
        }
        for inst in block.instruction_iter(context) {
            get_localised_id(inst, localised_value_id).hash(state);
            let inst = inst.get_instruction(context).unwrap();
            std::mem::discriminant(&inst.op).hash(state);
            // Hash value inputs to instructions in one-go.
            for v in inst.op.get_operands() {
                hash_value(context, v, localised_value_id, state);
            }
            // Hash non-value inputs.
            match &inst.op {
                crate::InstOp::AsmBlock(asm_block, args) => {
                    for arg in args
                        .iter()
                        .map(|arg| &arg.name)
                        .chain(asm_block.args_names.iter())
                    {
                        arg.as_str().hash(state);
                    }
                    if let Some(return_name) = &asm_block.return_name {
                        return_name.as_str().hash(state);
                    }
                    asm_block.return_type.hash(state);
                    for asm_inst in &asm_block.body {
                        asm_inst.op_name.as_str().hash(state);
                        for arg in &asm_inst.args {
                            arg.as_str().hash(state);
                        }
                        if let Some(imm) = &asm_inst.immediate {
                            imm.as_str().hash(state);
                        }
                    }
                }
                crate::InstOp::UnaryOp { op, .. } => op.hash(state),
                crate::InstOp::BinaryOp { op, .. } => op.hash(state),
                crate::InstOp::BitCast(_, ty) => ty.hash(state),
                crate::InstOp::Branch(b) => {
                    get_localised_id(b.block, localised_block_id).hash(state)
                }

                crate::InstOp::Call(callee, _) => {
                    match eq_class.function_hash_map.get(callee) {
                        Some(callee_hash) => {
                            callee_hash.hash(state);
                        }
                        None => {
                            // We haven't processed this callee yet. Just hash its name.
                            callee.get_name(context).hash(state);
                        }
                    }
                }
                crate::InstOp::CastPtr(_, ty) => ty.hash(state),
                crate::InstOp::Cmp(p, _, _) => p.hash(state),
                crate::InstOp::ConditionalBranch {
                    cond_value: _,
                    true_block,
                    false_block,
                } => {
                    get_localised_id(true_block.block, localised_block_id).hash(state);
                    get_localised_id(false_block.block, localised_block_id).hash(state);
                }
                crate::InstOp::ContractCall {
                    return_type, name, ..
                } => {
                    return_type.hash(state);
                    name.hash(state);
                }
                crate::InstOp::FuelVm(fuel_vm_inst) => match fuel_vm_inst {
                    crate::FuelVmInstruction::Gtf { tx_field_id, .. } => tx_field_id.hash(state),
                    crate::FuelVmInstruction::Log { log_ty, .. } => log_ty.hash(state),
                    crate::FuelVmInstruction::ReadRegister(reg) => reg.hash(state),
                    crate::FuelVmInstruction::Revert(_)
                    | crate::FuelVmInstruction::JmpbSsp(_)
                    | crate::FuelVmInstruction::Smo { .. }
                    | crate::FuelVmInstruction::StateClear { .. }
                    | crate::FuelVmInstruction::StateLoadQuadWord { .. }
                    | crate::FuelVmInstruction::StateLoadWord(_)
                    | crate::FuelVmInstruction::StateStoreQuadWord { .. }
                    | crate::FuelVmInstruction::StateStoreWord { .. } => (),
                    crate::FuelVmInstruction::WideUnaryOp { op, .. } => op.hash(state),
                    crate::FuelVmInstruction::WideBinaryOp { op, .. } => op.hash(state),
                    crate::FuelVmInstruction::WideModularOp { op, .. } => op.hash(state),
                    crate::FuelVmInstruction::WideCmpOp { op, .. } => op.hash(state),
                },
                crate::InstOp::GetLocal(local) => function
                    .lookup_local_name(context, local)
                    .unwrap()
                    .hash(state),
                crate::InstOp::GetElemPtr { elem_ptr_ty, .. } => elem_ptr_ty.hash(state),
                crate::InstOp::IntToPtr(_, ty) => ty.hash(state),
                crate::InstOp::Load(_) => (),
                crate::InstOp::MemCopyBytes { byte_len, .. } => byte_len.hash(state),
                crate::InstOp::MemCopyVal { .. } | crate::InstOp::Nop => (),
                crate::InstOp::PtrToInt(_, ty) => ty.hash(state),
                crate::InstOp::Ret(_, ty) => ty.hash(state),
                crate::InstOp::Store { .. } => (),
            }
        }
    }

    state.finish()
}

pub fn dedup_fns(
    context: &mut Context,
    _: &AnalysisResults,
    module: Module,
) -> Result<bool, IrError> {
    let mut modified = false;
    let eq_class = &mut EqClass {
        hash_set_map: FxHashMap::default(),
        function_hash_map: FxHashMap::default(),
    };
    let cg = build_call_graph(context, &context.modules.get(module.0).unwrap().functions);
    let callee_first = callee_first_order(&cg);
    for function in callee_first {
        let hash = hash_fn(context, function, eq_class);
        eq_class
            .hash_set_map
            .entry(hash)
            .and_modify(|class| {
                class.insert(function);
            })
            .or_insert(vec![function].into_iter().collect());
        eq_class.function_hash_map.insert(function, hash);
    }

    // Let's go over the entire module, replacing calls to functions
    // with their representatives in the equivalence class.
    for function in module.function_iter(context) {
        let mut replacements = vec![];
        for (_block, inst) in function.instruction_iter(context) {
            let Some(Instruction {
                op: InstOp::Call(callee, args),
                ..
            }) = inst.get_instruction(context)
            else {
                continue;
            };
            let Some(callee_hash) = eq_class.function_hash_map.get(callee) else {
                continue;
            };
            // If the representative (first element in the set) is different, we need to replace.
            let Some(callee_rep) = eq_class
                .hash_set_map
                .get(callee_hash)
                .and_then(|f| f.iter().next())
                .filter(|rep| *rep != callee)
            else {
                continue;
            };
            replacements.push((inst, args.clone(), callee_rep));
        }
        if !replacements.is_empty() {
            modified = true;
        }
        for (inst, args, callee_rep) in replacements {
            inst.replace(
                context,
                crate::ValueDatum::Instruction(Instruction {
                    op: InstOp::Call(*callee_rep, args.clone()),
                    parent: inst.get_instruction(context).unwrap().parent,
                }),
            );
        }
    }

    Ok(modified)
}
