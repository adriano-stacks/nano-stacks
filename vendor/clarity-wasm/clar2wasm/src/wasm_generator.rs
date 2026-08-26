use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::ops::Deref;
use std::rc::Rc;

use clarity::vm::analysis::ContractAnalysis;
use clarity::vm::diagnostic::DiagnosableError;
use clarity::vm::functions::define::DefineFunctions;
use clarity::vm::types::signatures::{CallableSubtype, StringUTF8Length};
use clarity::vm::types::{
    ASCIIData, CharType, FixedFunction, FunctionType, ListTypeData, PrincipalData,
    QualifiedContractIdentifier, SequenceData, SequenceSubtype, StringSubtype, TraitIdentifier,
    TupleTypeSignature, TypeSignature,
};
use clarity::vm::variables::NativeVariables;
use clarity::vm::{
    functions, variables, ClarityName, ClarityVersion, SymbolicExpression, SymbolicExpressionType,
};
use walrus::ir::{
    BinaryOp, IfElse, InstrSeqId, InstrSeqType, LoadKind, MemArg, StoreKind, UnaryOp,
};
use walrus::{
    ActiveData, DataKind, FunctionBuilder, FunctionId, GlobalId, InstrSeqBuilder, LocalId,
    MemoryId, Module, ValType,
};

use crate::cost::{ChargeContext, ChargeGenerator, WordCharge};
use crate::duck_type::need_ducktyping;
use crate::error_mapping::ErrorMap;
use crate::wasm_utils::{
    admit_preserves, get_type_in_memory_size, get_type_size, signature_from_string,
    trait_identifier_as_bytes, ArgumentCountCheck, PRINCIPAL_BYTES_MAX,
};
use crate::{check_args, debug_msg, words};

// First free position after data directly defined in standard.wat
pub const END_OF_STANDARD_DATA: u32 = 1352;

/// WasmGenerator is a Clarity AST visitor that generates a WebAssembly module
/// as it traverses the AST.
pub struct WasmGenerator {
    /// The contract analysis, which contains the expressions and type
    /// information for the contract.
    pub(crate) contract_analysis: ContractAnalysis,
    /// Code-generation-only type refinements. The stored analysis remains the
    /// source of truth for consensus typing and arity reports.
    lowered_type_overrides: HashMap<u64, TypeSignature>,
    /// original map_types, used for cost computation were we need a 1:1 mapping of the original complete types
    pub(crate) map_types_original: BTreeMap<ClarityName, (TypeSignature, TypeSignature)>,
    /// The WebAssembly module that is being generated.
    pub(crate) module: Module,
    /// Offset of the end of the literal memory.
    pub(crate) literal_memory_end: u32,
    /// Global ID of the stack pointer.
    pub(crate) stack_pointer: GlobalId,
    /// Start of the fixed scratch area carrying actual argument sizes into a
    /// public or read-only function.
    argument_sizes: GlobalId,
    /// Number of entries reserved in `argument_sizes`.
    max_argument_sizes: usize,
    /// Map strings saved in the literal memory to their offset.
    pub(crate) literal_memory_offset: HashMap<LiteralMemoryEntry, u32>,
    /// Map constants to an offset in the literal memory.
    pub(crate) constants: HashMap<String, TypeSignature>,
    /// Map constants bound to a contract principal literal to their contract identifier.
    pub(crate) constant_contract_principals: HashMap<String, QualifiedContractIdentifier>,
    /// The current function body block, used for early exit
    pub(crate) early_return_block_id: Option<InstrSeqId>,
    /// The type of the current function.
    pub(crate) current_function_type: Option<FixedFunction>,
    /// Return-buffer parameter while emitting a memory-backed user function.
    pub(crate) packed_return_offset: Option<LocalId>,
    /// The types of defined data-vars
    pub(crate) datavars_types: HashMap<ClarityName, TypeSignature>,
    /// The types of (key, value) in defined maps
    pub(crate) maps_types: HashMap<ClarityName, (TypeSignature, TypeSignature)>,
    /// The type of defined NFTs
    pub(crate) nft_types: HashMap<ClarityName, TypeSignature>,
    /// The (offsets, lengths) of trait IDs
    pub(crate) used_traits: HashMap<TraitIdentifier, (u32, u32)>,
    /// The names of defined functions
    pub(crate) defined_functions: HashSet<String>,
    /// User functions, kept separate from same-named host and stdlib functions.
    user_functions: HashMap<ClarityName, FunctionId>,

    /// The locals for the current function.
    pub(crate) bindings: Bindings,

    /// Whether reading a bound value copies it, and so pays to do so.
    charge_local_value_copy: bool,
    /// The `SymbolicExpression` being traversed, for the `NANO_TRACE_CHARGES`
    /// probe only.
    ///
    /// A label says *what* was charged and an amount says how much; neither
    /// says *where*. Two engines that decompose the same work into different
    /// charge events can only be compared by position, which is why the probe
    /// reports this alongside each charge.
    charging_expression: u64,
    /// A short source form for [`Self::charging_expression`], so a trace names
    /// the expression instead of a number whose identifier walk has to be
    /// guessed at. Empty when nothing is being traversed.
    charging_form: String,

    /// Emits cost tracking code if set.
    pub(crate) cost_context: Option<ChargeContext>,

    // Global ID of the linked error
    pub(crate) linked_error: GlobalId,

    /// Size of the current function's stack frame.
    frame_size: i32,
    /// Size of the maximum extra work space required by the stdlib functions
    /// to be available on the stack.
    max_work_space: u32,
    local_pool: Rc<RefCell<LocalPool>>,
    /// Reusable locals allocated while emitting each nested expression.
    expression_locals: Vec<Vec<LocalId>>,
    /// Peak live locals measured per generated function.
    pub(crate) locals_report: Rc<RefCell<LocalsReport>>,
    /// Maximum flattened arities encountered before wide values are packed.
    pub(crate) arity_report: Rc<RefCell<ArityReport>>,
    /// Reads remaining per `let`/`match` binding, by binding id, counted by a
    /// pre-pass over the contract's expressions; a binding's locals return
    /// to the pool when its last read is emitted.
    pub(crate) binding_uses: Vec<u32>,
    /// The binding id introduced by each binding-name expression, by the
    /// expression's AST id.
    pub(crate) binding_ids: HashMap<u64, u32>,
    /// `let`/`match` bindings that spill to the frame, by the binding-name
    /// expression's AST id.
    pub(crate) spilled_bindings: HashSet<u64>,
    /// Spill-area bytes reserved at each function's entry, by function name
    /// (`.top-level` for the contract body).
    spill_sizes: HashMap<String, u32>,
    /// The frame pointer of the function whose body is being emitted, while
    /// it has a spill area.
    pub(crate) frame_pointer: Option<LocalId>,
    /// Next free byte in the current frame's spill area.
    pub(crate) spill_cursor: u32,
}

/// Maximum flattened slots kept in wasm locals for lexically live bindings.
/// The remaining validator headroom covers parameters and compiler
/// temporaries; bindings beyond this conservative budget live in the frame.
const BINDING_LOCAL_BUDGET: usize = 1_000;

/// Counts the reads of every lexically introduced binding (`let` and
/// `match` names) in the contract's expressions, so that code generation
/// can return a binding's locals to the pool at its last read. The walk
/// mirrors evaluation order, and code generation traverses each expression
/// exactly once, so a binding's count reaches zero at its last read
/// whichever order the two visits happen in. Function parameters are not
/// counted: they stay live for their whole body.
///
/// A second walk plans individual spills from the finalized use counts and
/// flattened slot widths. This catches one wide composite, cumulative nested
/// scopes, and `match` payloads as well as a flat, many-name `let`.
#[derive(Debug, Default)]
struct BindingUses {
    uses: Vec<u32>,
    ids: HashMap<u64, u32>,
    /// Bindings in scope during the walk, innermost last.
    scopes: HashMap<ClarityName, Vec<u32>>,
    /// Bindings whose values spill to the frame, by binding-name expression id.
    spilled: HashSet<u64>,
    /// Spill-area bytes to reserve at each function's entry, by function
    /// name (`.top-level` for the contract body).
    spill_sizes: HashMap<String, u32>,
}

impl BindingUses {
    fn compute(
        expressions: &[SymbolicExpression],
        get_ty: impl Fn(&SymbolicExpression) -> Option<TypeSignature>,
    ) -> Self {
        let mut uses = Self::default();
        for expr in expressions {
            uses.walk(expr, &get_ty);
        }
        let mut planner = SpillPlanner::new(&uses.uses, &uses.ids);
        for expr in expressions {
            planner.walk(expr, &get_ty);
        }
        uses.spilled = planner.spilled;
        uses.spill_sizes = planner.spill_sizes;
        uses
    }

    fn bind(&mut self, name_expr: &SymbolicExpression, name: &ClarityName) -> u32 {
        let id = self.uses.len() as u32;
        self.uses.push(0);
        self.ids.insert(name_expr.id, id);
        self.scopes.entry(name.clone()).or_default().push(id);
        id
    }

    fn unbind(&mut self, name: &ClarityName) {
        if let Some(ids) = self.scopes.get_mut(name) {
            ids.pop();
        }
    }

    fn walk(
        &mut self,
        expr: &SymbolicExpression,
        get_ty: &impl Fn(&SymbolicExpression) -> Option<TypeSignature>,
    ) {
        match &expr.expr {
            SymbolicExpressionType::Atom(name) => {
                if let Some(&id) = self.scopes.get(name).and_then(|ids| ids.last()) {
                    self.uses[id as usize] += 1;
                }
            }
            SymbolicExpressionType::List(list) => self.walk_list(list, get_ty),
            _ => {}
        }
    }

    fn walk_list(
        &mut self,
        list: &[SymbolicExpression],
        get_ty: &impl Fn(&SymbolicExpression) -> Option<TypeSignature>,
    ) {
        let Some((
            SymbolicExpression {
                expr: SymbolicExpressionType::Atom(head),
                ..
            },
            args,
        )) = list.split_first()
        else {
            // A list that does not begin with a word is not a call, and this
            // used to return without looking inside one. An allowance list is
            // exactly that shape -- `((with-ft SBTC "sbtc-token" total))` -- so
            // a binding read only from an allowance was counted zero times,
            // `let` dropped its value instead of saving it, and the read pushed
            // nothing: the allowance then took whatever was under it. Mainnet's
            // `SP28MP1HQ….keepgoing-safe` compiled to a module wasmtime refuses
            // for that reason, and the same miss with matching types would have
            // computed a wrong value in a module that loads. Counting too many
            // reads only keeps a slot alive; counting too few frees one that is
            // still read, so the walk goes on rather than stopping here.
            for expr in list {
                self.walk(expr, get_ty);
            }
            return;
        };
        match head.as_str() {
            // `let` bindings are visible to the values bound after them and
            // to the body.
            "let" => {
                let mut bound = Vec::new();
                if let Some(SymbolicExpression {
                    expr: SymbolicExpressionType::List(bindings),
                    ..
                }) = args.first()
                {
                    for pair in bindings {
                        if let SymbolicExpressionType::List(pair) = &pair.expr {
                            if let [name_expr, value] = pair.as_slice() {
                                self.walk(value, get_ty);
                                if let SymbolicExpressionType::Atom(name) = &name_expr.expr {
                                    let id = self.bind(name_expr, name);
                                    bound.push((name.clone(), id, get_ty(value)));
                                }
                            }
                        }
                    }
                }
                for expr in args.get(1..).unwrap_or(&[]) {
                    self.walk(expr, get_ty);
                }
                for (name, _, _) in bound.iter().rev() {
                    self.unbind(name);
                }
            }
            // `match` binds a name in each arm's body: the success name in
            // the first, and the error name too in a response match.
            "match" if args.len() == 4 || args.len() == 5 => {
                self.walk(&args[0], get_ty);
                if let SymbolicExpressionType::Atom(name) = &args[1].expr {
                    self.bind(&args[1], name);
                    self.walk(&args[2], get_ty);
                    self.unbind(name);
                } else {
                    self.walk(&args[2], get_ty);
                }
                if args.len() == 4 {
                    self.walk(&args[3], get_ty);
                } else if let SymbolicExpressionType::Atom(name) = &args[3].expr {
                    self.bind(&args[3], name);
                    self.walk(&args[4], get_ty);
                    self.unbind(name);
                } else {
                    self.walk(&args[4], get_ty);
                }
            }
            "define-public" | "define-read-only" | "define-private" => {
                for expr in args {
                    self.walk(expr, get_ty);
                }
            }
            _ => {
                for expr in list {
                    self.walk(expr, get_ty);
                }
            }
        }
    }
}

struct SpillPlanner<'a> {
    uses: &'a [u32],
    ids: &'a HashMap<u64, u32>,
    spilled: HashSet<u64>,
    spill_sizes: HashMap<String, u32>,
    current_fn: String,
    live_slots: usize,
}

impl<'a> SpillPlanner<'a> {
    fn new(uses: &'a [u32], ids: &'a HashMap<u64, u32>) -> Self {
        Self {
            uses,
            ids,
            spilled: HashSet::new(),
            spill_sizes: HashMap::new(),
            current_fn: ".top-level".to_owned(),
            live_slots: 0,
        }
    }

    fn plan_binding(&mut self, name: &SymbolicExpression, ty: Option<&TypeSignature>) -> usize {
        let Some(id) = self.ids.get(&name.id).copied() else {
            return 0;
        };
        if self.uses.get(id as usize).copied().unwrap_or(0) == 0 {
            return 0;
        }
        let Some(ty) = ty else { return 0 };
        let slots = clar2wasm_ty(ty).len();
        if self.live_slots.saturating_add(slots) <= BINDING_LOCAL_BUDGET {
            self.live_slots += slots;
            return slots;
        }
        self.spilled.insert(name.id);
        let bytes = u32::try_from(get_type_size(ty)).unwrap_or(u32::MAX);
        let size = self.spill_sizes.entry(self.current_fn.clone()).or_default();
        *size = size.saturating_add(bytes);
        0
    }

    fn walk(
        &mut self,
        expr: &SymbolicExpression,
        get_ty: &impl Fn(&SymbolicExpression) -> Option<TypeSignature>,
    ) {
        let SymbolicExpressionType::List(list) = &expr.expr else {
            return;
        };
        let Some((head, args)) = list.split_first() else {
            return;
        };
        let SymbolicExpressionType::Atom(head) = &head.expr else {
            for item in list {
                self.walk(item, get_ty);
            }
            return;
        };
        match head.as_str() {
            "let" => {
                let mut local_slots = 0;
                if let Some(SymbolicExpression {
                    expr: SymbolicExpressionType::List(bindings),
                    ..
                }) = args.first()
                {
                    for pair in bindings {
                        let SymbolicExpressionType::List(pair) = &pair.expr else {
                            continue;
                        };
                        let [name, value] = pair.as_slice() else {
                            continue;
                        };
                        self.walk(value, get_ty);
                        local_slots += self.plan_binding(name, get_ty(value).as_ref());
                    }
                }
                for body in args.get(1..).unwrap_or(&[]) {
                    self.walk(body, get_ty);
                }
                self.live_slots = self.live_slots.saturating_sub(local_slots);
            }
            "match" if args.len() == 4 || args.len() == 5 => {
                self.walk(&args[0], get_ty);
                let match_type = get_ty(&args[0]);
                let (success_ty, error_ty) = match match_type.as_ref() {
                    Some(TypeSignature::OptionalType(inner)) => (Some(inner.as_ref()), None),
                    Some(TypeSignature::ResponseType(inner)) => (Some(&inner.0), Some(&inner.1)),
                    _ => (None, None),
                };
                // Response code generation captures both payloads before it
                // selects a branch, so both count as live at the same point.
                let error_slots = if args.len() == 5 {
                    self.plan_binding(&args[3], error_ty)
                } else {
                    0
                };
                let success_slots = self.plan_binding(&args[1], success_ty);
                self.walk(&args[2], get_ty);
                self.walk(args.last().expect("match has a final branch"), get_ty);
                self.live_slots = self
                    .live_slots
                    .saturating_sub(success_slots.saturating_add(error_slots));
            }
            "define-public" | "define-read-only" | "define-private" => {
                let outer_fn = self.current_fn.clone();
                let outer_slots = std::mem::replace(&mut self.live_slots, 0);
                if let Some(SymbolicExpression {
                    expr: SymbolicExpressionType::List(signature),
                    ..
                }) = args.first()
                {
                    if let Some(SymbolicExpression {
                        expr: SymbolicExpressionType::Atom(name),
                        ..
                    }) = signature.first()
                    {
                        self.current_fn = name.as_str().to_owned();
                    }
                }
                for item in args {
                    self.walk(item, get_ty);
                }
                self.current_fn = outer_fn;
                self.live_slots = outer_slots;
            }
            _ => {
                for item in list {
                    self.walk(item, get_ty);
                }
            }
        }
    }
}

#[derive(Debug, Clone, Default)]
pub(crate) struct Bindings {
    values: HashMap<ClarityName, InnerBindings>,
    depth: u32,
}
#[derive(Debug, Clone)]
struct InnerBindings {
    storage: BindingStorage,
    ty: TypeSignature,
    /// The binding's id in the use count pre-pass, for `let`/`match`
    /// bindings. Function parameters carry none: they stay live for their
    /// whole body.
    binding: Option<u32>,
}

/// Where a binding's value lives.
#[derive(Debug, Clone)]
pub(crate) enum BindingStorage {
    Locals(Vec<LocalId>),
    /// A constant byte offset from a memory base. This covers planned frame
    /// spills and packed function parameters without flattening either into
    /// wasm locals.
    Memory {
        base: LocalId,
        delta: u32,
    },
}

impl Bindings {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    pub(crate) fn insert_spilled(
        &mut self,
        name: ClarityName,
        ty: TypeSignature,
        storage: BindingStorage,
        binding: Option<u32>,
    ) {
        self.values.insert(
            name,
            InnerBindings {
                storage,
                ty,
                binding,
            },
        );
    }

    pub(crate) fn enter_scope(&mut self) -> Result<(), GeneratorError> {
        self.depth = self
            .depth
            .checked_add(1)
            .ok_or_else(|| GeneratorError::InternalError("binding depth overflow".to_owned()))?;
        Ok(())
    }

    pub(crate) const fn depth(&self) -> u32 {
        self.depth
    }

    pub(crate) fn contains(&self, name: &ClarityName) -> bool {
        self.values.contains_key(name)
    }

    pub(crate) fn get_locals_and_type(
        &self,
        name: &ClarityName,
    ) -> Option<(BindingStorage, TypeSignature, Option<u32>)> {
        self.values
            .get(name)
            .map(|binding| (binding.storage.clone(), binding.ty.clone(), binding.binding))
    }

    pub(crate) fn get_trait_identifier(&self, name: &ClarityName) -> Option<&TraitIdentifier> {
        self.values.get(name).and_then(|b| match &b.ty {
            TypeSignature::CallableType(CallableSubtype::Trait(t)) => Some(t),
            _ => None,
        })
    }
}

/// A one-line source form for an expression, for the charge probe's positions.
///
/// Enough to recognise which expression a charge belongs to without counting
/// identifiers by hand: a name for an atom, the head and arity for a call, and
/// the kind for anything else.
fn source_form(expr: &SymbolicExpression) -> String {
    match &expr.expr {
        SymbolicExpressionType::Atom(name) => name.to_string(),
        SymbolicExpressionType::LiteralValue(_) => "<literal>".to_owned(),
        SymbolicExpressionType::List(items) => items
            .first()
            .and_then(|head| head.match_atom())
            .map_or_else(
                || format!("(list of {})", items.len()),
                |head| format!("({head} /{})", items.len().saturating_sub(1)),
            ),
        _ => "<other>".to_owned(),
    }
}

/// How a value's dynamic type size — the term a tuple's size includes for its
/// own type — can be obtained.
///
/// The reference sizes a value through `type_of`, so a tuple carrying `none`
/// contributes `NoType` where the declaration says `(optional (string-ascii
/// 40))`. Reading the declared type there overcharges by the difference.
#[derive(Clone, Copy, PartialEq, Eq)]
enum TypeSizePlan {
    /// The dynamic type is the declared one, so the declared size is exact.
    Declared,
    /// An optional or response picks between arms of different type size, and
    /// the generated code can branch on the discriminant it already holds.
    Measured,
    /// Only the host, materializing the value, can answer.
    Host,
}

/// Which plan measures this type's dynamic type size *as a tuple field*.
///
/// A composite is admitted only as `Declared`: measuring one means reading its
/// fields' discriminants, and a tuple or list whose runtime-shape handle is set
/// keeps its value in the arena, where those locals no longer describe it. An
/// optional or a response is safe because measuring it reads only its own
/// discriminant and then a constant.
fn type_size_plan(ty: &TypeSignature) -> TypeSizePlan {
    match ty {
        TypeSignature::NoType
        | TypeSignature::IntType
        | TypeSignature::UIntType
        | TypeSignature::BoolType
        | TypeSignature::PrincipalType
        | TypeSignature::SequenceType(
            SequenceSubtype::BufferType(_) | SequenceSubtype::StringType(_),
        )
        // A callable's type size is 1 whatever it points at, so the declaration
        // is exact — and it must not be measured by materialising, because a
        // trait reference erases to a bare principal on the way through memory
        // and comes back sized 148 where the reference says 276.
        | TypeSignature::CallableType(_)
        | TypeSignature::TraitReferenceType(_)
        | TypeSignature::ListUnionType(_) => TypeSizePlan::Declared,
        TypeSignature::OptionalType(inner) => match type_size_plan(inner) {
            TypeSizePlan::Host => TypeSizePlan::Host,
            _ => TypeSizePlan::Measured,
        },
        TypeSignature::ResponseType(response) => {
            if type_size_plan(&response.0) == TypeSizePlan::Host
                || type_size_plan(&response.1) == TypeSizePlan::Host
            {
                TypeSizePlan::Host
            } else {
                TypeSizePlan::Measured
            }
        }
        TypeSignature::TupleType(tuple) => {
            if tuple
                .get_type_map()
                .values()
                .all(|field| type_size_plan(field) == TypeSizePlan::Declared)
            {
                TypeSizePlan::Declared
            } else {
                TypeSizePlan::Host
            }
        }
        // The same element types the inline list measurement admits: each is
        // its own dynamic type and sizes like `NoType`, which is what makes the
        // empty list — whose entry type the reference derives as `NoType` —
        // agree. Any other element needs the least-supertype fold.
        TypeSignature::SequenceType(SequenceSubtype::ListType(list)) => {
            if matches!(
                list.get_list_item_type(),
                TypeSignature::IntType
                    | TypeSignature::UIntType
                    | TypeSignature::BoolType
                    | TypeSignature::PrincipalType
            ) {
                TypeSizePlan::Declared
            } else {
                TypeSizePlan::Host
            }
        }
    }
}

/// Whether a value of this type has a runtime-shape handle in its first slot.
fn carries_runtime_shape(ty: &TypeSignature) -> bool {
    matches!(
        ty,
        TypeSignature::TupleType(_) | TypeSignature::SequenceType(SequenceSubtype::ListType(_))
    )
}

/// Whether a binding of this type resolves through the callable context.
///
/// In a Clarity 1 contract a trait argument lives *only* in the callable
/// context, and the reference hands it back owned — so reading it never pays
/// `LookupVariableSize`, only the depth charge. From Clarity 2 the argument
/// is also inserted as an ordinary variable and pays the copy like any other.
fn is_trait_reference(ty: &TypeSignature) -> bool {
    matches!(
        ty,
        TypeSignature::CallableType(CallableSubtype::Trait(_))
            | TypeSignature::TraitReferenceType(_)
    )
}

#[derive(Hash, Eq, PartialEq)]
pub enum LiteralMemoryEntry {
    Ascii(String),
    Utf8(String),
    Bytes(Box<[u8]>),
}

#[derive(Debug)]
pub enum GeneratorError {
    /// A shape code generation has no case for, and *which* shape.
    ///
    /// It carried nothing, so every one of the eight sites that raises it
    /// produced the same three words. Three mainnet contracts refuse to compile
    /// with exactly that message and task 093 has to reduce them; "Not
    /// implemented" does not say what to reduce towards.
    NotImplemented(String),
    InternalError(String),
    TypeError(String),
    ArgumentCountMismatch,
}

pub enum FunctionKind {
    Public,
    Private,
    ReadOnly,
}

impl DiagnosableError for GeneratorError {
    fn message(&self) -> String {
        match self {
            GeneratorError::NotImplemented(what) => format!("Not implemented: {what}"),
            GeneratorError::InternalError(msg) => format!("Internal error: {msg}"),
            GeneratorError::TypeError(msg) => format!("Type error: {msg}"),
            GeneratorError::ArgumentCountMismatch => "Argument count mismatch".to_string(),
        }
    }

    fn suggestion(&self) -> Option<String> {
        None
    }
}

pub trait ArgumentsExt {
    fn get_expr(&self, n: usize) -> Result<&SymbolicExpression, GeneratorError>;
    fn get_name(&self, n: usize) -> Result<&ClarityName, GeneratorError>;
    fn get_list(&self, n: usize) -> Result<&[SymbolicExpression], GeneratorError>;
}

impl ArgumentsExt for &[SymbolicExpression] {
    fn get_expr(&self, n: usize) -> Result<&SymbolicExpression, GeneratorError> {
        self.get(n).ok_or_else(|| {
            GeneratorError::InternalError(format!(
                "{self:?} does not have an argument of index {n}"
            ))
        })
    }

    fn get_name(&self, n: usize) -> Result<&ClarityName, GeneratorError> {
        self.get_expr(n)?.match_atom().ok_or_else(|| {
            GeneratorError::InternalError(format!(
                "{self:?} does not have a name at argument index {n}"
            ))
        })
    }

    fn get_list(&self, n: usize) -> Result<&[SymbolicExpression], GeneratorError> {
        self.get_expr(n)?.match_list().ok_or_else(|| {
            GeneratorError::InternalError(format!(
                "{self:?} does not have a list at argument index {n}"
            ))
        })
    }
}

/// Push a placeholder value for Wasm type `ty` onto the data stack.
/// `unreachable!` is used for Wasm types that should never be used.
#[allow(clippy::unreachable)]
pub(crate) fn add_placeholder_for_type(builder: &mut InstrSeqBuilder, ty: ValType) {
    match ty {
        ValType::I32 => builder.i32_const(0),
        ValType::I64 => builder.i64_const(0),
        ValType::F32 | ValType::F64 | ValType::V128 | ValType::Externref | ValType::Funcref => {
            unreachable!("Use of Wasm type {}", ty);
        }
    };
}

/// Push a placeholder value for Clarity type `ty` onto the data stack.
pub(crate) fn add_placeholder_for_clarity_type(builder: &mut InstrSeqBuilder, ty: &TypeSignature) {
    let wasm_types = clar2wasm_ty(ty);
    for wasm_type in wasm_types.iter() {
        add_placeholder_for_type(builder, *wasm_type);
    }
}

/// Convert a Clarity type signature to a wasm type signature.
pub(crate) fn clar2wasm_ty(ty: &TypeSignature) -> Vec<ValType> {
    match ty {
        TypeSignature::NoType => vec![ValType::I32], // TODO: can this just be empty?
        TypeSignature::IntType => vec![ValType::I64, ValType::I64],
        TypeSignature::UIntType => vec![ValType::I64, ValType::I64],
        TypeSignature::ResponseType(inner_types) => {
            let mut types = vec![ValType::I32];
            types.extend(clar2wasm_ty(&inner_types.0));
            types.extend(clar2wasm_ty(&inner_types.1));
            types
        }
        TypeSignature::SequenceType(SequenceSubtype::ListType(_)) => vec![
            ValType::I32, // runtime-shape handle
            ValType::I32, // offset
            ValType::I32, // length
        ],
        TypeSignature::SequenceType(_) | TypeSignature::ListUnionType(_) => vec![
            ValType::I32, // offset
            ValType::I32, // length
        ],
        TypeSignature::BoolType => vec![ValType::I32],
        TypeSignature::PrincipalType
        | TypeSignature::CallableType(_)
        | TypeSignature::TraitReferenceType(_) => vec![
            ValType::I32, // offset
            ValType::I32, // length
        ],
        TypeSignature::OptionalType(inner_ty) => {
            let mut types = vec![ValType::I32];
            types.extend(clar2wasm_ty(inner_ty));
            types
        }
        TypeSignature::TupleType(inner_types) => {
            let mut types = vec![ValType::I32]; // runtime-shape handle
            for inner_type in inner_types.get_type_map().values() {
                types.extend(clar2wasm_ty(inner_type));
            }
            types
        }
    }
}

/// Number of Wasm slots in the source-level flattened representation, before
/// compiler-only hidden metadata and memory-backed lowering.
pub(crate) fn source_wasm_arity(ty: &TypeSignature) -> usize {
    match ty {
        TypeSignature::NoType | TypeSignature::BoolType => 1,
        TypeSignature::IntType | TypeSignature::UIntType => 2,
        TypeSignature::ResponseType(inner_types) => {
            1 + source_wasm_arity(&inner_types.0) + source_wasm_arity(&inner_types.1)
        }
        TypeSignature::SequenceType(_)
        | TypeSignature::ListUnionType(_)
        | TypeSignature::PrincipalType
        | TypeSignature::CallableType(_)
        | TypeSignature::TraitReferenceType(_) => 2,
        TypeSignature::OptionalType(inner_ty) => 1 + source_wasm_arity(inner_ty),
        TypeSignature::TupleType(inner_types) => inner_types
            .get_type_map()
            .values()
            .map(source_wasm_arity)
            .sum(),
    }
}

/// wasmparser rejects function and block types above this many inputs or outputs.
pub const MAX_WASM_TYPE_ARITY: usize = 1_000;

pub(crate) fn uses_packed_slots(params: usize, results: usize) -> bool {
    params > MAX_WASM_TYPE_ARITY || results > MAX_WASM_TYPE_ARITY
}

/// Whether a Clarity value is too wide to cross a Wasm function or block type.
pub(crate) fn uses_packed_value(ty: &TypeSignature) -> bool {
    uses_packed_slots(0, clar2wasm_ty(ty).len())
}

pub(crate) fn has_runtime_shape(ty: &TypeSignature) -> bool {
    match ty {
        TypeSignature::OptionalType(inner) => has_runtime_shape(inner),
        TypeSignature::ResponseType(inner) => {
            has_runtime_shape(&inner.0) || has_runtime_shape(&inner.1)
        }
        TypeSignature::TupleType(_) | TypeSignature::SequenceType(SequenceSubtype::ListType(_)) => {
            true
        }
        _ => false,
    }
}

/// Functions beyond either Wasm boundary pass their values through linear memory.
pub(crate) fn uses_packed_abi(function: &FixedFunction) -> bool {
    let params = function
        .args
        .iter()
        .map(|arg| clar2wasm_ty(&arg.signature).len())
        .sum::<usize>();
    uses_packed_slots(params, clar2wasm_ty(&function.returns).len())
}

/// One step of reading a stored value out as a wider type.
///
/// A value laid out for a type carrying `NoType` placeholders has fewer wasm
/// slots than the same shape with those placeholders resolved — `(optional
/// NoType)` is an indicator and one `i32`, `(optional uint)` an indicator and
/// two `i64`s. The placeholder slot is discarded and zeros stand in for the
/// value that is not there, which the indicator already says.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Widen {
    /// Read the next stored slot.
    Take,
    /// Skip a placeholder slot, which has no counterpart.
    Skip,
    /// Push a zero the stored value has no slot for.
    Zero(ValType),
}

/// How to read a value stored as `stored` where `expected` is wanted.
///
/// `None` when the two are not the same shape, which is a real type error
/// rather than a placeholder to fill in.
pub(crate) fn widen_actions(
    stored: &TypeSignature,
    expected: &TypeSignature,
) -> Option<Vec<Widen>> {
    if stored == expected {
        return Some(vec![Widen::Take; clar2wasm_ty(stored).len()]);
    }
    match (stored, expected) {
        // The placeholder itself: its slot goes, the real layout arrives.
        (TypeSignature::NoType, _) => {
            let mut actions = vec![Widen::Skip];
            actions.extend(clar2wasm_ty(expected).into_iter().map(Widen::Zero));
            Some(actions)
        }
        (TypeSignature::OptionalType(inside), TypeSignature::OptionalType(wanted)) => {
            let mut actions = vec![Widen::Take];
            actions.extend(widen_actions(inside, wanted)?);
            Some(actions)
        }
        (TypeSignature::ResponseType(inside), TypeSignature::ResponseType(wanted)) => {
            let mut actions = vec![Widen::Take];
            actions.extend(widen_actions(&inside.0, &wanted.0)?);
            actions.extend(widen_actions(&inside.1, &wanted.1)?);
            Some(actions)
        }
        (TypeSignature::TupleType(inside), TypeSignature::TupleType(wanted)) => {
            let (inside, wanted) = (inside.get_type_map(), wanted.get_type_map());
            if inside.len() != wanted.len() {
                return None;
            }
            let mut actions = vec![Widen::Take];
            for ((left_name, left), (right_name, right)) in inside.iter().zip(wanted.iter()) {
                if left_name != right_name {
                    return None;
                }
                actions.extend(widen_actions(left, right)?);
            }
            Some(actions)
        }
        // A sequence is an offset and a length whatever it holds, so an empty
        // list needs nothing done to it — which is why `(list)` in the same
        // position never produced this failure.
        _ => (clar2wasm_ty(stored) == clar2wasm_ty(expected))
            .then(|| vec![Widen::Take; clar2wasm_ty(stored).len()]),
    }
}

#[derive(Debug)]
pub enum SequenceElementType {
    /// A byte, from a string-ascii or buffer.
    Byte,
    /// A 32-bit unicode scalar value, from a string-utf8.
    UnicodeScalar,
    /// Any other type.
    Other(TypeSignature),
}

impl SequenceElementType {
    pub fn type_size(&self) -> i32 {
        match self {
            SequenceElementType::Byte => 1,
            SequenceElementType::UnicodeScalar => 4,
            SequenceElementType::Other(ty) => get_type_size(ty),
        }
    }

    pub fn load(
        &self,
        generator: &mut WasmGenerator,
        builder: &mut InstrSeqBuilder,
        offset: LocalId,
    ) -> Result<(), GeneratorError> {
        match self {
            SequenceElementType::Byte => {
                builder.local_get(offset).i32_const(1);
            }
            SequenceElementType::UnicodeScalar => {
                builder.local_get(offset).i32_const(4);
            }
            SequenceElementType::Other(type_signature) => {
                generator.read_from_memory(builder, offset, 0, type_signature)?;
            }
        }
        Ok(())
    }
}

impl TryFrom<&TypeSignature> for SequenceElementType {
    type Error = GeneratorError;

    fn try_from(ty: &TypeSignature) -> Result<Self, Self::Error> {
        match ty {
            TypeSignature::SequenceType(SequenceSubtype::ListType(lt)) => {
                Ok(SequenceElementType::Other(lt.get_list_item_type().clone()))
            }
            TypeSignature::SequenceType(SequenceSubtype::BufferType(_))
            | TypeSignature::SequenceType(SequenceSubtype::StringType(StringSubtype::ASCII(_))) => {
                Ok(SequenceElementType::Byte)
            }
            TypeSignature::SequenceType(SequenceSubtype::StringType(StringSubtype::UTF8(_))) => {
                Ok(SequenceElementType::UnicodeScalar)
            }
            _ => Err(GeneratorError::TypeError(
                "expected sequence type".to_string(),
            )),
        }
    }
}

impl From<&SequenceElementType> for TypeSignature {
    fn from(se: &SequenceElementType) -> Self {
        match se {
            SequenceElementType::Other(o) => o.clone(),
            // Techically, a Byte could also be a (string-ascii 1), but not having this distinction makes
            // the code cleaner where this function is used.
            SequenceElementType::Byte => TypeSignature::BUFFER_1.clone(),
            SequenceElementType::UnicodeScalar => {
                TypeSignature::SequenceType(SequenceSubtype::StringType(StringSubtype::UTF8(
                    #[allow(clippy::unwrap_used)]
                    StringUTF8Length::try_from(1u32).unwrap(),
                )))
            }
        }
    }
}

/// Drop a value of type `ty` from the data stack.
pub(crate) fn drop_value(builder: &mut InstrSeqBuilder, ty: &TypeSignature) {
    let wasm_types = clar2wasm_ty(ty);
    (0..wasm_types.len()).for_each(|_| {
        builder.drop();
    });
}

pub fn get_global(module: &Module, name: &str) -> Result<GlobalId, GeneratorError> {
    module
        .globals
        .iter()
        .find(|global| {
            global
                .name
                .as_ref()
                .is_some_and(|other_name| name == other_name)
        })
        .map(|global| global.id())
        .ok_or_else(|| {
            GeneratorError::InternalError(format!("Expected to find a global named ${name}"))
        })
}

fn get_function(module: &Module, name: &str) -> Result<FunctionId, GeneratorError> {
    module.funcs.by_name(name).ok_or_else(|| {
        GeneratorError::InternalError(format!("Expected to find a function named ${name}"))
    })
}

pub struct BorrowedLocal {
    id: LocalId,
    ty: ValType,
    pool: Rc<RefCell<LocalPool>>,
}

impl Drop for BorrowedLocal {
    fn drop(&mut self) {
        (*self.pool).borrow_mut().give_back(self.ty, self.id);
    }
}

impl Deref for BorrowedLocal {
    type Target = LocalId;
    fn deref(&self) -> &Self::Target {
        &self.id
    }
}

/// Locals released by dead scopes and temporaries, available for reuse, plus
/// a running count of the live ones so the peak a function reaches is
/// measurable at compile time.
#[derive(Debug, Default)]
pub(crate) struct LocalPool {
    free: HashMap<ValType, Vec<LocalId>>,
    in_use: HashSet<LocalId>,
    live: u32,
    max_live: u32,
}

impl LocalPool {
    fn take(&mut self, ty: ValType) -> Option<LocalId> {
        let local = self.free.get_mut(&ty).and_then(Vec::pop);
        if let Some(local) = local {
            self.in_use.insert(local);
            self.note_live(1);
            return Some(local);
        }
        None
    }

    fn add(&mut self, local: LocalId) {
        self.in_use.insert(local);
        self.note_live(1);
    }

    fn note_live(&mut self, count: u32) {
        self.live += count;
        self.max_live = self.max_live.max(self.live);
    }

    fn give_back(&mut self, ty: ValType, local: LocalId) {
        if !self.in_use.remove(&local) {
            return;
        }
        self.live -= 1;
        self.free.entry(ty).or_default().push(local);
    }

    /// Give a generated function its own local namespace. Locals are scoped to
    /// one Wasm function, so reusing a `LocalId` from the contract body or a
    /// previously generated function is invalid even when its Rust lifetime
    /// has ended.
    fn enter_function(&mut self) -> Self {
        std::mem::take(self)
    }

    fn max_live(&self) -> u32 {
        self.max_live
    }

    /// Restore the enclosing function's namespace and return this function's
    /// peak.
    fn leave_function(&mut self, outer: Self) -> u32 {
        let function = std::mem::replace(self, outer);
        function.max_live
    }
}

/// Local use in a compiled contract.
///
/// `max_live_locals` is the compiler pool's peak and is useful when evaluating
/// reuse. `emitted` is parsed from the final Wasm bytes and is the exact count
/// the validator sees. This is measurement only: nothing refuses compilation
/// based on it.
#[derive(Debug, Clone, Default)]
pub struct LocalsReport {
    pub max_live_locals: HashMap<String, u32>,
    /// Exact parameter and declared-local counts parsed from the emitted Wasm.
    pub emitted: HashMap<String, EmittedLocals>,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct EmittedLocals {
    pub parameters: u32,
    pub declared: u32,
    pub total: u32,
}

impl LocalsReport {
    /// Measure the module bytes the runtime will validate.
    pub fn measure_emitted(&mut self, wasm: &[u8]) -> Result<(), String> {
        use wasmparser::{ExternalKind, Name, NameSectionReader, Parser, Payload, TypeRef};

        let mut parameter_counts = Vec::new();
        let mut imported_functions = 0_u32;
        let mut function_types = Vec::new();
        let mut declared_locals = Vec::new();
        let mut names = HashMap::new();
        let mut exports = HashMap::new();

        for payload in Parser::new(0).parse_all(wasm) {
            match payload.map_err(|error| error.to_string())? {
                Payload::TypeSection(types) => {
                    for ty in types.into_iter_err_on_gc_types() {
                        let parameters =
                            u32::try_from(ty.map_err(|error| error.to_string())?.params().len())
                                .map_err(|_| "function parameter count exceeds u32".to_owned())?;
                        parameter_counts.push(parameters);
                    }
                }
                Payload::ImportSection(imports) => {
                    for import in imports {
                        if matches!(
                            import.map_err(|error| error.to_string())?.ty,
                            TypeRef::Func(_)
                        ) {
                            imported_functions = imported_functions
                                .checked_add(1)
                                .ok_or_else(|| "imported function count exceeds u32".to_owned())?;
                        }
                    }
                }
                Payload::FunctionSection(functions) => {
                    for function in functions {
                        function_types.push(function.map_err(|error| error.to_string())?);
                    }
                }
                Payload::CodeSectionEntry(body) => {
                    let mut declared = 0_u32;
                    for local in body
                        .get_locals_reader()
                        .map_err(|error| error.to_string())?
                    {
                        let (count, _) = local.map_err(|error| error.to_string())?;
                        declared = declared
                            .checked_add(count)
                            .ok_or_else(|| "declared local count exceeds u32".to_owned())?;
                    }
                    declared_locals.push(declared);
                }
                Payload::ExportSection(section) => {
                    for export in section {
                        let export = export.map_err(|error| error.to_string())?;
                        if export.kind == ExternalKind::Func {
                            exports.insert(export.index, export.name.to_owned());
                        }
                    }
                }
                Payload::CustomSection(section) if section.name() == "name" => {
                    for subsection in NameSectionReader::new(section.data(), section.data_offset())
                    {
                        if let Name::Function(map) =
                            subsection.map_err(|error| error.to_string())?
                        {
                            for naming in map {
                                let naming = naming.map_err(|error| error.to_string())?;
                                names.insert(naming.index, naming.name.to_owned());
                            }
                        }
                    }
                }
                _ => {}
            }
        }

        if function_types.len() != declared_locals.len() {
            return Err(format!(
                "function/code section mismatch: {} types for {} bodies",
                function_types.len(),
                declared_locals.len()
            ));
        }
        self.emitted.clear();
        for (defined_index, (type_index, declared)) in
            function_types.into_iter().zip(declared_locals).enumerate()
        {
            let function_index = imported_functions
                .checked_add(
                    u32::try_from(defined_index)
                        .map_err(|_| "defined function count exceeds u32".to_owned())?,
                )
                .ok_or_else(|| "function index exceeds u32".to_owned())?;
            let parameters = *parameter_counts.get(type_index as usize).ok_or_else(|| {
                format!("function {function_index} has unknown type {type_index}")
            })?;
            let total = parameters
                .checked_add(declared)
                .ok_or_else(|| format!("function {function_index} local total exceeds u32"))?;
            let base = names
                .get(&function_index)
                .or_else(|| exports.get(&function_index))
                .cloned()
                .unwrap_or_else(|| format!("function-{function_index}"));
            let label = if self.emitted.contains_key(&base) {
                format!("{base}#{function_index}")
            } else {
                base
            };
            self.emitted.insert(
                label,
                EmittedLocals {
                    parameters,
                    declared,
                    total,
                },
            );
        }
        Ok(())
    }
}

/// Maximum flattened WebAssembly arities in a compiled Clarity contract.
///
/// These are the source-level widths before memory-backed lowering, so a
/// contract inventory can measure its margin without inspecting emitted Wasm.
#[derive(Debug, Clone, Default, Eq, PartialEq)]
pub struct ArityReport {
    pub max_function_params: usize,
    pub max_function_results: usize,
    pub max_control_params: usize,
    pub max_control_results: usize,
    pub top_level_results: usize,
}

impl WasmGenerator {
    fn charge_reserved_variable_fetch(
        &self,
        builder: &mut InstrSeqBuilder,
    ) -> Result<(), GeneratorError> {
        self.charge(builder, ClarityName::from_literal("var-get"), 1_u32)
    }

    pub fn new(contract_analysis: ContractAnalysis) -> Result<WasmGenerator, GeneratorError> {
        let standard_lib_wasm: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/standard.wasm"));

        let mut module = Module::from_buffer(standard_lib_wasm).map_err(|_err| {
            GeneratorError::InternalError("failed to load standard library".to_owned())
        })?;
        let save_shape_ty = module
            .types
            .add(&[ValType::I32, ValType::I32, ValType::I32], &[ValType::I32]);
        let (save_shape, _) =
            module.add_import_func("clarity", "save_runtime_shape", save_shape_ty);
        module.funcs.get_mut(save_shape).name = Some("stdlib.save_runtime_shape".to_owned());
        let shape_size_ty = module.types.add(&[ValType::I32], &[ValType::I32]);
        let (shape_size, _) =
            module.add_import_func("clarity", "runtime_shape_serialization_size", shape_size_ty);
        module.funcs.get_mut(shape_size).name =
            Some("stdlib.runtime_shape_serialization_size".to_owned());
        let (value_size, _) =
            module.add_import_func("clarity", "runtime_value_size", save_shape_ty);
        module.funcs.get_mut(value_size).name = Some("stdlib.runtime_value_size".to_owned());
        let (element_size, _) =
            module.add_import_func("clarity", "runtime_sequence_element_size", save_shape_ty);
        module.funcs.get_mut(element_size).name =
            Some("stdlib.runtime_sequence_element_size".to_owned());
        let merge_shape_ty = module.types.add(
            &[ValType::I32, ValType::I32, ValType::I32, ValType::I32],
            &[ValType::I32],
        );
        let (merge_shape, _) =
            module.add_import_func("clarity", "merge_runtime_shape", merge_shape_ty);
        module.funcs.get_mut(merge_shape).name = Some("stdlib.merge_runtime_shape".to_owned());
        let (deserialize_shape, _) =
            module.add_import_func("clarity", "deserialize_runtime_shape", merge_shape_ty);
        module.funcs.get_mut(deserialize_shape).name =
            Some("stdlib.deserialize_runtime_shape".to_owned());
        let (field_shape, _) =
            module.add_import_func("clarity", "field_runtime_shape", save_shape_ty);
        module.funcs.get_mut(field_shape).name = Some("stdlib.field_runtime_shape".to_owned());
        let (handle_size, _) =
            module.add_import_func("clarity", "runtime_shape_size", shape_size_ty);
        module.funcs.get_mut(handle_size).name = Some("stdlib.runtime_shape_size".to_owned());
        let save_filtered_shape_ty = module.types.add(
            &[
                ValType::I32,
                ValType::I32,
                ValType::I32,
                ValType::I32,
                ValType::I32,
            ],
            &[ValType::I32],
        );
        let (save_filtered_shape, _) = module.add_import_func(
            "clarity",
            "save_filtered_runtime_shape",
            save_filtered_shape_ty,
        );
        module.funcs.get_mut(save_filtered_shape).name =
            Some("stdlib.save_filtered_runtime_shape".to_owned());
        let admit_argument_ty = module.types.add(
            &[
                ValType::I32,
                ValType::I32,
                ValType::I32,
                ValType::I32,
                ValType::I32,
                ValType::I32,
            ],
            &[],
        );
        let (admit_argument, _) =
            module.add_import_func("clarity", "admit_function_argument", admit_argument_ty);
        module.funcs.get_mut(admit_argument).name =
            Some("stdlib.admit_function_argument".to_owned());
        let shape_equal_ty = module.types.add(
            &[ValType::I32, ValType::I32, ValType::I32, ValType::I32],
            &[ValType::I32],
        );
        let (shape_equal, _) =
            module.add_import_func("clarity", "runtime_shape_is_equal", shape_equal_ty);
        module.funcs.get_mut(shape_equal).name = Some("stdlib.runtime_shape_is_equal".to_owned());
        // Get the stack-pointer global ID
        let global_id = get_global(&module, "stack-pointer")?;
        let argument_sizes = module.globals.add_local(
            ValType::I32,
            false,
            walrus::InitExpr::Value(walrus::ir::Value::I32(0)),
        );
        module.exports.add("argument-sizes", argument_sizes);

        let linked_error_id = get_global(&module, "runtime-error-linked")?;

        Ok(WasmGenerator {
            map_types_original: contract_analysis.map_types.clone(),
            contract_analysis,
            lowered_type_overrides: HashMap::new(),
            module,
            literal_memory_end: END_OF_STANDARD_DATA,
            stack_pointer: global_id,
            argument_sizes,
            max_argument_sizes: 0,
            linked_error: linked_error_id,
            literal_memory_offset: HashMap::new(),
            constants: HashMap::new(),
            constant_contract_principals: HashMap::new(),
            bindings: Bindings::new(),
            charge_local_value_copy: true,
            charging_expression: 0,
            charging_form: String::new(),
            cost_context: None,
            early_return_block_id: None,
            current_function_type: None,
            packed_return_offset: None,
            frame_size: 0,
            max_work_space: 0,
            datavars_types: HashMap::new(),
            maps_types: HashMap::new(),
            local_pool: Rc::new(RefCell::new(LocalPool::default())),
            expression_locals: Vec::new(),
            locals_report: Rc::new(RefCell::new(LocalsReport::default())),
            arity_report: Rc::new(RefCell::new(ArityReport::default())),
            binding_uses: Vec::new(),
            binding_ids: HashMap::new(),
            spilled_bindings: HashSet::new(),
            spill_sizes: HashMap::new(),
            frame_pointer: None,
            spill_cursor: 0,
            nft_types: HashMap::new(),
            used_traits: HashMap::new(),
            defined_functions: HashSet::new(),
            user_functions: HashMap::new(),
        })
    }

    pub fn with_cost_code(contract_analysis: ContractAnalysis) -> Result<Self, GeneratorError> {
        let epoch: clarity::types::StacksEpochId = contract_analysis.epoch;
        Self::with_cost_code_for_epoch(contract_analysis, epoch)
    }

    /// Generate cost code from a table that is not the contract's own epoch.
    ///
    /// A contract keeps the semantics of the epoch it was written for — that is
    /// what its stored analysis means — but the chain charges it at the rate of
    /// the epoch it is running in. Recompiling a contract an older epoch
    /// accepted would otherwise price every call into it wrongly.
    pub fn with_cost_code_for_epoch(
        contract_analysis: ContractAnalysis,
        cost_epoch: clarity::types::StacksEpochId,
    ) -> Result<Self, GeneratorError> {
        let mut generator = Self::new(contract_analysis)?;

        let module = &mut generator.module;

        // The meters live in the standard library as module-defined exported
        // globals — store-owned imports would force the host to build them
        // per call, which is the one thing standing between it and a
        // pre-resolved instantiation. Generated code charges against the
        // standard module's own globals.
        let r = get_global(module, "cost-runtime")?;
        let rc = get_global(module, "cost-read-count")?;
        let rl = get_global(module, "cost-read-length")?;
        let wc = get_global(module, "cost-write-count")?;
        let wl = get_global(module, "cost-write-length")?;

        let charge_probe = if std::env::var_os("NANO_TRACE_CHARGES").is_some() {
            let probe_type = module.types.add(&[ValType::I32, ValType::I64], &[]);
            let (probe, _) = module.add_import_func("clarity", "charge_probe", probe_type);
            Some(probe)
        } else {
            None
        };
        generator.cost_context = Some(ChargeContext {
            epoch: cost_epoch,
            charge_probe,
            runtime: r,
            read_count: rc,
            read_length: rl,
            write_count: wc,
            write_length: wl,
            runtime_error: get_function(module, "stdlib.runtime-error")?,
        });
        Ok(generator)
    }

    /// Whether data words charge the bytes the database actually moved, the
    /// rule every epoch from 2.05 on uses, rather than 2.0's static type
    /// sizes.
    ///
    /// This is a property of [`Self::executing_epoch`] — the epoch the chain
    /// is running now — never of the semantic epoch the contract keeps. The
    /// interpreter decides the same branch from its runtime epoch, so a
    /// contract written for 2.0 and running in a later epoch pays
    /// serialized-size charges like everything else. Deciding it from
    /// `contract_analysis.epoch` froze 2.0 charging into recompiled old
    /// contracts and inflated three cost dimensions on mainnet (task 146).
    /// Without a charging epoch no charge code is emitted at all, so the
    /// fallback to the semantic epoch only picks the shape of the
    /// (non-charging) generated code.
    /// Whether an atom of this type resolves through the callable context and
    /// comes back owned, paying no `LookupVariableSize`. True only for trait
    /// references in Clarity 1 contracts; see [`is_trait_reference`].
    fn reads_owned_callable(&self, ty: &TypeSignature) -> bool {
        is_trait_reference(ty) && self.contract_analysis.clarity_version < ClarityVersion::Clarity2
    }

    pub(crate) fn charges_serialized_sizes(&self) -> bool {
        self.executing_epoch()
            .unwrap_or(self.contract_analysis.epoch)
            >= clarity::types::StacksEpochId::Epoch2_05
    }

    pub fn set_memory_pages(&mut self) -> Result<(), GeneratorError> {
        let memory = self
            .module
            .memories
            .iter_mut()
            .next()
            .ok_or_else(|| GeneratorError::InternalError("No Memory found".to_owned()))?;

        let total_memory_bytes =
            self.literal_memory_end + (self.frame_size as u32) + self.max_work_space;
        let pages_required = total_memory_bytes / (64 * 1024);
        let remainder = total_memory_bytes % (64 * 1024);

        memory.initial = pages_required + (remainder > 0) as u32;

        Ok(())
    }

    pub fn generate(mut self) -> Result<Module, GeneratorError> {
        self.register_defined_traits()?;
        let expressions = std::mem::take(&mut self.contract_analysis.expressions);

        // Count the reads of every `let`/`match` binding, so that code
        // generation can return a binding's locals to the pool at its last
        // read instead of keeping them to the end of its scope, and mark the
        // scopes wide enough that their bindings spill to the frame.
        let binding_uses =
            BindingUses::compute(&expressions, |expr| self.get_expr_type(expr).cloned());
        self.binding_uses = binding_uses.uses;
        self.binding_ids = binding_uses.ids;
        self.spilled_bindings = binding_uses.spilled;
        self.spill_sizes = binding_uses.spill_sizes;

        // Get the type of the last top-level expression with a return value.
        let return_type = expressions
            .iter()
            .rev()
            .find_map(|expr| self.get_expr_type(expr))
            .cloned();
        let flattened_results = return_type.as_ref().map_or(0, source_wasm_arity);
        self.arity_report.borrow_mut().top_level_results = flattened_results;
        let packed_top_level = return_type.as_ref().is_some_and(uses_packed_value);
        let return_offset = packed_top_level.then(|| self.module.locals.add(ValType::I32));
        let params = return_offset.map_or_else(Vec::new, |_| vec![ValType::I32]);
        let results = if packed_top_level {
            Vec::new()
        } else {
            return_type.as_ref().map_or_else(Vec::new, clar2wasm_ty)
        };
        if let Some(return_type) = return_type.as_ref().filter(|_| packed_top_level) {
            self.frame_size += get_type_size(return_type);
        }

        let mut current_function = FunctionBuilder::new(&mut self.module.types, &params, &results);

        if !expressions.is_empty() {
            let mut body = current_function.func_body();

            // The contract body is a frame of its own: reserve its spill
            // area below the working stack.
            let spill_size = self.spill_sizes.get(".top-level").copied().unwrap_or(0);
            if spill_size > 0 {
                let frame_pointer = self.module.locals.add(ValType::I32);
                body.global_get(self.stack_pointer)
                    .local_set(frame_pointer)
                    .global_get(self.stack_pointer)
                    .i32_const(spill_size as i32)
                    .binop(BinaryOp::I32Add)
                    .global_set(self.stack_pointer);
                self.frame_pointer = Some(frame_pointer);
                self.frame_size += spill_size as i32;
            }

            self.traverse_statement_list(&mut body, &expressions)?;
            if let (Some(return_offset), Some(return_type)) = (return_offset, &return_type) {
                self.write_to_memory(&mut body, return_offset, 0, return_type)?;
            }
        }

        // Defined functions save and restore the live-local counts around
        // their own generation, so the peak left here is the top-level's own.
        let peak = (*self.local_pool).borrow().max_live();
        self.locals_report
            .borrow_mut()
            .max_live_locals
            .insert(".top-level".to_owned(), peak);

        self.contract_analysis.expressions = expressions;

        let top_level =
            current_function.finish(return_offset.into_iter().collect(), &mut self.module.funcs);
        self.module.exports.add(".top-level", top_level);

        let argument_sizes_offset = self.literal_memory_end;
        let argument_sizes_length = u32::try_from(self.max_argument_sizes)
            .ok()
            .and_then(|length| length.checked_mul(4))
            .ok_or_else(|| {
                GeneratorError::InternalError("function argument-size area is too large".into())
            })?;
        self.literal_memory_end = self
            .literal_memory_end
            .checked_add(argument_sizes_length)
            .ok_or_else(|| {
                GeneratorError::InternalError("literal memory offset overflow".into())
            })?;
        self.module.globals.get_mut(self.argument_sizes).kind = walrus::GlobalKind::Local(
            walrus::InitExpr::Value(walrus::ir::Value::I32(argument_sizes_offset as i32)),
        );

        self.set_memory_pages()?;

        // Update the initial value of the stack-pointer to point beyond the
        // literal memory.
        self.module.globals.get_mut(self.stack_pointer).kind = walrus::GlobalKind::Local(
            walrus::InitExpr::Value(walrus::ir::Value::I32(self.literal_memory_end as i32)),
        );
        // Create a global with the amount of workspace needed in this contract
        let workspace_global = self.module.globals.add_local(
            ValType::I32,
            false,
            walrus::InitExpr::Value(walrus::ir::Value::I32(
                self.frame_size + self.max_work_space as i32,
            )),
        );
        self.module.exports.add("workspace-size", workspace_global);

        Ok(self.module)
    }

    fn register_defined_traits(&mut self) -> Result<(), GeneratorError> {
        let contract_identifier = self.contract_analysis.contract_identifier.clone();
        let trait_names: Vec<_> = self
            .contract_analysis
            .defined_traits
            .keys()
            .cloned()
            .collect();

        for name in trait_names {
            let trait_identifier = TraitIdentifier {
                name,
                contract_identifier: contract_identifier.clone(),
            };
            let offset_length = self.add_trait_identifier(&trait_identifier)?;
            self.used_traits.insert(trait_identifier, offset_length);
        }

        Ok(())
    }

    pub fn get_memory(&self) -> Result<MemoryId, GeneratorError> {
        Ok(self
            .module
            .memories
            .iter()
            .next()
            .ok_or(GeneratorError::InternalError("No memory found".to_owned()))?
            .id())
    }

    pub fn traverse_expr(
        &mut self,
        builder: &mut InstrSeqBuilder,
        expr: &SymbolicExpression,
    ) -> Result<(), GeneratorError> {
        self.expression_locals.push(Vec::new());
        let enclosing = std::mem::replace(&mut self.charging_expression, expr.id);
        let enclosing_form = std::mem::replace(&mut self.charging_form, source_form(expr));
        let result = match &expr.expr {
            SymbolicExpressionType::Atom(name) => self.visit_atom(builder, expr, name),
            SymbolicExpressionType::List(exprs) => self.traverse_list(builder, expr, exprs),
            SymbolicExpressionType::LiteralValue(value) => {
                self.visit_literal_value(builder, expr, value)
            }
            _ => Ok(()),
        };
        let locals = self
            .expression_locals
            .pop()
            .expect("an expression local scope was pushed");
        self.release_locals(locals);
        self.charging_expression = enclosing;
        self.charging_form = enclosing_form;
        result
    }

    /// Where a charge emitted right now belongs, for the charge probe.
    ///
    /// The contract disambiguates: expression identifiers restart at one per
    /// contract, and a single transaction charges across several.
    pub(crate) fn charging_position(&self) -> String {
        format!(
            "{}#{} {}",
            self.contract_analysis.contract_identifier.name,
            self.charging_expression,
            self.charging_form
        )
    }

    /// Traverse an expression whose value the interpreter reads in place, so a
    /// bound name it names is not copied and does not pay to be.
    pub fn traverse_expr_without_value_copy_charge(
        &mut self,
        builder: &mut InstrSeqBuilder,
        expr: &SymbolicExpression,
    ) -> Result<(), GeneratorError> {
        let previous = std::mem::replace(&mut self.charge_local_value_copy, false);
        let result = self.traverse_expr(builder, expr);
        self.charge_local_value_copy = previous;
        result
    }

    pub fn traverse_expr_as_borrowed_value(
        &mut self,
        builder: &mut InstrSeqBuilder,
        expr: &SymbolicExpression,
    ) -> Result<(), GeneratorError> {
        if matches!(expr.expr, SymbolicExpressionType::Atom(_)) {
            self.traverse_expr_without_value_copy_charge(builder, expr)
        } else {
            self.traverse_expr(builder, expr)
        }
    }

    pub fn traverse_callable_reference(
        &mut self,
        builder: &mut InstrSeqBuilder,
        expr: &SymbolicExpression,
    ) -> Result<(), GeneratorError> {
        let SymbolicExpressionType::Atom(atom) = &expr.expr else {
            return Err(GeneratorError::TypeError(
                "callable reference must be an atom".to_owned(),
            ));
        };
        let (storage, ty, binding) = self.bindings.get_locals_and_type(atom).ok_or_else(|| {
            GeneratorError::InternalError(format!("unable to find local for {}", atom.as_str()))
        })?;
        let values = match storage {
            BindingStorage::Locals(values) => values,
            BindingStorage::Memory { base, delta } => {
                self.read_from_memory(builder, base, delta, &ty)?;
                let values = self.save_to_locals(builder, &ty, true);
                for value in &values {
                    builder.local_get(*value);
                }
                self.release_locals(values);
                return Ok(());
            }
        };
        for value in &values {
            builder.local_get(*value);
        }
        self.note_binding_read(binding, &values);
        Ok(())
    }

    fn traverse_list(
        &mut self,
        builder: &mut InstrSeqBuilder,
        expr: &SymbolicExpression,
        list: &[SymbolicExpression],
    ) -> Result<(), GeneratorError> {
        self.traverse_list_with_function_lookup(builder, expr, list, true)
    }

    pub(crate) fn traverse_allowance_expr(
        &mut self,
        builder: &mut InstrSeqBuilder,
        expr: &SymbolicExpression,
    ) -> Result<(), GeneratorError> {
        match &expr.expr {
            SymbolicExpressionType::List(list) => {
                self.traverse_list_with_function_lookup(builder, expr, list, false)
            }
            _ => self.traverse_expr(builder, expr),
        }
    }

    fn traverse_list_with_function_lookup(
        &mut self,
        builder: &mut InstrSeqBuilder,
        expr: &SymbolicExpression,
        list: &[SymbolicExpression],
        charge_function_lookup: bool,
    ) -> Result<(), GeneratorError> {
        match list.split_first() {
            Some((
                SymbolicExpression {
                    expr: SymbolicExpressionType::Atom(function_name),
                    ..
                },
                args,
            )) => {
                // Extract the types from the args and return
                let get_types = || {
                    let arg_types: Result<Vec<TypeSignature>, GeneratorError> = args
                        .iter()
                        .map(|e| {
                            self.get_expr_type(e).cloned().ok_or_else(|| {
                                GeneratorError::TypeError("expected valid argument type".to_owned())
                            })
                        })
                        .collect();
                    let return_type = self
                        .get_expr_type(expr)
                        .ok_or_else(|| {
                            GeneratorError::TypeError("Simple words must be typed".to_owned())
                        })
                        .cloned();
                    Ok((arg_types?, return_type?))
                };

                // A definition is not evaluated, so only an application resolves
                // a name to a function and pays for doing so.
                if charge_function_lookup
                    && DefineFunctions::lookup_by_name(function_name).is_none()
                {
                    self.charge_function_lookup(builder)?;
                }

                let calls_user_function = functions::lookup_reserved_functions(
                    function_name.as_str(),
                    &self.contract_analysis.clarity_version,
                )
                .is_none()
                    && self.is_user_defined_function(function_name.as_str());

                // Complex words handle their own argument traversal, and have priority
                // since we need to have a slight overlap for the words `and` and `or`
                // which exist in both complex and simple forms
                if calls_user_function {
                    self.traverse_call_user_defined(builder, expr, function_name, args)?;
                } else if let Some(word) = words::lookup_complex(function_name) {
                    word.traverse(self, builder, expr, args)?;
                } else if let Some(simpleword) = words::lookup_simple(function_name) {
                    let (arg_types, return_type) = get_types()?;

                    // traverse arguments
                    let borrowed = simpleword.reads_operands_in_place();
                    for arg in args {
                        if borrowed {
                            self.traverse_expr_as_borrowed_value(builder, arg)?;
                        } else {
                            self.traverse_expr(builder, arg)?;
                        }
                    }

                    simpleword.visit(self, builder, &arg_types, &return_type)?;
                } else if let Some(variadic) = words::lookup_variadic_simple(function_name) {
                    let (arg_types, return_type) = get_types()?;

                    // The interpreter evaluates every argument, then charges
                    // once through `dispatch_args`, then folds them pairwise.
                    // Charging before the arguments makes an expression that
                    // aborts part-way pay for work it never did, which is
                    // invisible while everything succeeds and wrong the moment
                    // something does not.
                    if args.is_empty() {
                        return Err(GeneratorError::InternalError(
                            "Variadic called without arguments".to_owned(),
                        ));
                    }
                    let mut evaluated = Vec::with_capacity(args.len());
                    for (expr, ty) in args.iter().zip(arg_types.iter()) {
                        self.traverse_expr(builder, expr)?;
                        evaluated.push(self.save_to_locals(builder, ty, true));
                    }

                    variadic.charge(self, builder, arg_types.len() as u32)?;

                    for local in &evaluated[0] {
                        builder.local_get(*local);
                    }
                    if arg_types.len() == 1 {
                        variadic.visit(self, builder, &arg_types[..1], &return_type)?;
                    } else {
                        for (i, locals) in evaluated.iter().enumerate().skip(1) {
                            for local in locals {
                                builder.local_get(*local);
                            }
                            variadic.visit(self, builder, &arg_types[i - 1..=i], &return_type)?;
                        }
                    }

                    // Every argument was consumed by its visit; the slots the
                    // evaluated arguments were saved in are dead.
                    for locals in evaluated {
                        self.release_locals(locals);
                    }

                    // first argument is traversed outside loop
                } else {
                    self.traverse_call_user_defined(builder, expr, function_name, args)?;
                }
            }
            _ => return Err(GeneratorError::InternalError("Invalid list".into())),
        }
        Ok(())
    }

    pub fn traverse_define_function(
        &mut self,
        builder: &mut InstrSeqBuilder,
        name: &ClarityName,
        body: &SymbolicExpression,
        kind: FunctionKind,
    ) -> Result<FunctionId, GeneratorError> {
        let opt_function_type = match kind {
            FunctionKind::ReadOnly => {
                builder.i32_const(0);
                self.contract_analysis
                    .get_read_only_function_type(name.as_str())
            }
            FunctionKind::Public => {
                builder.i32_const(1);
                self.contract_analysis
                    .get_public_function_type(name.as_str())
            }
            FunctionKind::Private => {
                builder.i32_const(2);
                self.contract_analysis.get_private_function(name.as_str())
            }
        };
        let function_type = if let Some(FunctionType::Fixed(fixed)) = opt_function_type {
            fixed.clone()
        } else {
            return Err(GeneratorError::TypeError(match opt_function_type {
                Some(_) => "expected fixed function type".to_string(),
                None => format!("unable to find function type for {}", name.as_str()),
            }));
        };
        self.max_argument_sizes = self.max_argument_sizes.max(function_type.args.len());

        self.current_function_type = Some(function_type.clone());
        {
            let mut report = self.arity_report.borrow_mut();
            let params = function_type
                .args
                .iter()
                .map(|argument| source_wasm_arity(&argument.signature))
                .sum::<usize>();
            report.max_function_params = report.max_function_params.max(params);
            report.max_function_results = report
                .max_function_results
                .max(source_wasm_arity(&function_type.returns));
        }
        let packed_abi = uses_packed_abi(&function_type);

        // Count live locals from zero for this function; the caller's counts
        // (the top-level's, for a define) are restored on the way out.
        let outer_pool = (*self.local_pool).borrow_mut().enter_function();

        // Call the host interface to save this function
        // Arguments are kind (already pushed) and name (offset, length)
        let (id_offset, id_length) = self.add_string_literal(name)?;
        builder
            .i32_const(id_offset as i32)
            .i32_const(id_length as i32);

        // Call the host interface function, `define_function`
        builder.call(self.func_by_name("stdlib.define_function"));

        let mut bindings = Bindings::new();

        // Setup the parameters
        let mut param_locals = Vec::new();
        let mut params_types = Vec::new();
        let mut parameters = Vec::new();
        let mut reused_arg = None;
        let packed_offsets = packed_abi.then(|| {
            let arguments = self.module.locals.add(ValType::I32);
            let result = self.module.locals.add(ValType::I32);
            param_locals.extend([arguments, result]);
            params_types.extend([ValType::I32, ValType::I32]);
            (arguments, result)
        });
        let mut packed_argument_offset = 0_u32;
        for param in function_type.args.iter() {
            // Interpreter returns the first reused arg as NameAlreadyUsed argument
            if reused_arg.is_none() && bindings.contains(&param.name) {
                reused_arg = Some(param.name.clone());
            }

            let storage = if let Some((arguments, _)) = packed_offsets {
                let delta = packed_argument_offset;
                packed_argument_offset = packed_argument_offset
                    .checked_add(u32::try_from(get_type_size(&param.signature)).map_err(|_| {
                        GeneratorError::InternalError(
                            "negative packed parameter representation size".to_owned(),
                        )
                    })?)
                    .ok_or_else(|| {
                        GeneratorError::InternalError(
                            "packed parameter representation overflow".to_owned(),
                        )
                    })?;
                BindingStorage::Memory {
                    base: arguments,
                    delta,
                }
            } else {
                let param_types = clar2wasm_ty(&param.signature);
                let mut locals = Vec::with_capacity(param_types.len());
                for ty in param_types {
                    let local = self.module.locals.add(ty);
                    locals.push(local);
                    param_locals.push(local);
                    params_types.push(ty);
                }
                BindingStorage::Locals(locals)
            };
            // A public function receives a trait argument as a bare principal.
            let value_ty = if matches!(&kind, FunctionKind::Public)
                && matches!(
                    &param.signature,
                    TypeSignature::CallableType(CallableSubtype::Trait(_))
                ) {
                TypeSignature::PrincipalType
            } else {
                param.signature.clone()
            };
            // Parameters are not counted by the use pre-pass: they stay live
            // for the whole body.
            bindings.insert_spilled(
                param.name.clone(),
                param.signature.clone(),
                storage.clone(),
                None,
            );
            parameters.push((param.signature.clone(), value_ty, storage));
        }

        // A call from outside writes this function's arguments into this
        // module's memory before entering it. Nothing else reserves that room,
        // so without this the call runs on whatever the page round-up spares —
        // enough until an argument is large enough that it is not.
        if matches!(kind, FunctionKind::Public | FunctionKind::ReadOnly) {
            self.frame_size += parameters
                .iter()
                .map(|(signature, _, _)| get_type_in_memory_size(signature, true))
                .sum::<i32>();
            if packed_abi {
                self.frame_size += parameters
                    .iter()
                    .map(|(signature, _, _)| get_type_size(signature))
                    .sum::<i32>()
                    + get_type_size(&function_type.returns);
            }
        }

        let results_types = if packed_abi {
            Vec::new()
        } else {
            clar2wasm_ty(&function_type.returns)
        };
        let mut func_builder = FunctionBuilder::new(
            &mut self.module.types,
            params_types.as_slice(),
            results_types.as_slice(),
        );
        func_builder.name(name.as_str().to_string());
        let mut func_body = func_builder.func_body();

        // Function prelude
        // Save the frame pointer in a local variable.
        let frame_pointer = self.module.locals.add(ValType::I32);
        func_body
            .global_get(self.stack_pointer)
            .local_set(frame_pointer);

        // Reserve the spill area wide `let` scopes keep their bindings in,
        // below the working frame, and make it visible to the body. The
        // enclosing frame's spill state is saved and restored around the
        // body, so a spilled scope in it keeps its offsets.
        let saved_frame = self.frame_pointer.take();
        let saved_cursor = std::mem::replace(&mut self.spill_cursor, 0);
        let spill_size = self.spill_sizes.get(name.as_str()).copied().unwrap_or(0);
        if spill_size > 0 {
            func_body
                .global_get(self.stack_pointer)
                .i32_const(spill_size as i32)
                .binop(BinaryOp::I32Add)
                .global_set(self.stack_pointer);
            self.frame_size += spill_size as i32;
            self.frame_pointer = Some(frame_pointer);
        }

        // Entering the function type-checks every argument it was given.
        self.charge_user_function_application(&mut func_body, function_type.args.len() as u32)?;
        let memory = self.get_memory()?;
        let uses_argument_value_size = self
            .executing_epoch()
            .is_some_and(|epoch| epoch.uses_arg_size_for_cost());
        for (index, (parameter_type, _, _)) in parameters.iter().enumerate() {
            let size = self.borrow_local(ValType::I32);
            if uses_argument_value_size {
                let offset = u32::try_from(index)
                    .ok()
                    .and_then(|index| index.checked_mul(4))
                    .ok_or_else(|| {
                        GeneratorError::InternalError(
                            "function argument-size offset overflow".into(),
                        )
                    })?;
                func_body
                    .global_get(self.argument_sizes)
                    .load(
                        memory,
                        LoadKind::I32 { atomic: false },
                        MemArg { align: 4, offset },
                    )
                    .local_set(*size);
            } else {
                let declared_size = i32::try_from(parameter_type.size().map_err(|error| {
                    GeneratorError::TypeError(format!(
                        "function parameter size is not realizable: {error}"
                    ))
                })?)
                .map_err(|_| {
                    GeneratorError::TypeError("function parameter size exceeds i32".into())
                })?;
                func_body.i32_const(declared_size).local_set(*size);
            }
            self.charge_inner_type_check(&mut func_body, *size)?;
        }

        // Clarity 2+ casts, sanitizes and admits every argument only after it
        // has charged the complete application. Static slots alone cannot do
        // that for a tuple or list whose hidden handle carries a wider runtime
        // shape, so reconstruct those arguments through the shared host
        // boundary before the body can read its bindings.
        if self.contract_analysis.clarity_version >= ClarityVersion::Clarity2 {
            for (index, (parameter_type, _, storage)) in parameters.iter().enumerate() {
                // A parameter whose declared type admission cannot change —
                // no tuple, no callable anywhere in it, so no hidden handle
                // can carry a wider shape — needs no reconstruction at all:
                // the whole round trip through the host was the identity.
                if has_runtime_shape(parameter_type) && !admit_preserves(parameter_type) {
                    self.admit_runtime_shape_parameter(
                        &mut func_body,
                        parameter_type,
                        storage,
                        id_offset,
                        id_length,
                        index,
                    )?;
                }
            }
        }

        // Setup the locals map for this function, saving the top-level map to
        // restore after.
        let top_level_locals = std::mem::replace(&mut self.bindings, bindings);

        // How deep the sender and caller stacks are before this function runs.
        //
        // `as-contract` pushes on entry and pops on exit, but an early return
        // out of its body — `asserts!` or `try!` inside it — branches straight
        // past the pop, and the switched sender is then inherited by whatever
        // runs next. Recording the depth here and unwinding to it in the
        // postlude below makes that impossible on every path, rather than
        // relying on the body reaching its own `exit_as_contract`.
        //
        // Mainnet block 8,668,161 is the case: a function that `asserts!` its
        // way out of `as-contract`, called twice by `map`, whose second call
        // then transferred to itself.
        // Only where the body can actually switch the sender. Two `i32` locals and
        // two calls on *every* function is a cost every contract on the chain pays
        // for a thing almost none of them do, and locals are not free: a function's
        // total is what wasmparser caps at 50,000, which is the limit task 073 is
        // about. A body with no `as-contract` in it cannot leave the stacks deeper
        // than it found them, so there is nothing to unwind.
        let switches_sender = body_contains_as_contract(body);
        let depths = switches_sender.then(|| {
            let sender_depth = self.module.locals.add(ValType::I32);
            let caller_depth = self.module.locals.add(ValType::I32);
            func_body
                .call(self.func_by_name("stdlib.principal_depth"))
                .local_set(caller_depth)
                .local_set(sender_depth);
            (sender_depth, caller_depth)
        });

        // The parameters, frame pointer and sender/caller depths bypass the
        // pool but are live for the whole function, so they count towards
        // its peak.
        let binding_locals = parameters
            .iter()
            .map(|(_, _, storage)| match storage {
                BindingStorage::Locals(locals) => locals.len() as u32,
                BindingStorage::Memory { .. } => 0,
            })
            .sum::<u32>();
        (*self.local_pool).borrow_mut().note_live(
            binding_locals + u32::from(packed_abi) * 2 + 1 + u32::from(switches_sender) * 2,
        );

        self.note_control_arity(0, source_wasm_arity(&function_type.returns));
        let block_type = self.checked_control_type(&[], results_types.as_slice())?;
        let mut block = func_body.dangling_instr_seq(block_type);
        let block_id = block.id();

        self.early_return_block_id = Some(block_id);
        self.packed_return_offset = packed_offsets.map(|(_, result)| result);

        // Traverse the body of the function
        self.set_expr_type(body, function_type.returns.clone())?;
        self.traverse_expr(&mut block, body)?;
        if let Some(return_offset) = self.packed_return_offset {
            self.write_to_memory(&mut block, return_offset, 0, &function_type.returns)?;
        }

        // If the same arg name is used multiple times, the interpreter throws an
        // `Unchecked` error at runtime, so we do the same here
        if let Some(arg_name) = reused_arg {
            let (arg_name_offset, arg_name_len) =
                self.add_clarity_string_literal(&CharType::ASCII(ASCIIData {
                    data: arg_name.as_bytes().to_vec(),
                }))?;

            // Clear function body
            block.instrs_mut().clear();

            block
                .i32_const(arg_name_offset as i32)
                .global_set(get_global(&self.module, "runtime-error-arg-offset")?)
                .i32_const(arg_name_len as i32)
                .global_set(get_global(&self.module, "runtime-error-arg-len")?)
                .i32_const(ErrorMap::NameAlreadyUsed as i32)
                .call(self.func_by_name("stdlib.runtime-error"))
                // To avoid having to generate correct return values
                .unreachable();
        }

        // Insert the function body block into the function
        func_body.instr(walrus::ir::Block { seq: block_id });

        // Function postlude
        // Restore the initial stack pointer.
        func_body
            .local_get(frame_pointer)
            .global_set(self.stack_pointer);

        // And the sender and caller stacks, which an early return out of
        // `as-contract` would otherwise leave deeper than it found them. Emitted
        // only where the prologue that records them was.
        if let Some((sender_depth, caller_depth)) = depths {
            func_body
                .local_get(sender_depth)
                .local_get(caller_depth)
                .call(self.func_by_name("stdlib.restore_principal_depth"));
        }
        // Restore the top-level locals map.
        self.bindings = top_level_locals;

        // And the enclosing frame's spill state.
        self.frame_pointer = saved_frame;
        self.spill_cursor = saved_cursor;

        // Reset the return type and early block to None
        self.current_function_type = None;
        self.early_return_block_id = None;
        self.packed_return_offset = None;

        // Record this function's peak and hand the counts back to the caller.
        let peak = (*self.local_pool).borrow_mut().leave_function(outer_pool);
        self.locals_report
            .borrow_mut()
            .max_live_locals
            .insert(name.as_str().to_string(), peak);

        let function = func_builder.finish(param_locals, &mut self.module.funcs);
        self.user_functions.insert(name.clone(), function);
        Ok(function)
    }

    /// Generates the wasm code for a ShortReturn error.
    ///
    /// It takes for the `runtime_error`
    /// argument either a [ErrorMap::ShortReturnAssertionFailure], a
    /// [ErrorMap::ShortReturnExpectedValue], a [ErrorMap::ShortReturnExpectedValueResponse]
    /// or a [ErrorMap::ShortReturnExpectedValueOptional].
    pub(crate) fn short_return_error(
        &mut self,
        builder: &mut InstrSeqBuilder,
        ty: &TypeSignature,
        runtime_error: ErrorMap,
    ) -> Result<(), GeneratorError> {
        match runtime_error {
            ErrorMap::ShortReturnAssertionFailure
            | ErrorMap::ShortReturnExpectedValue
            | ErrorMap::ShortReturnExpectedValueResponse => {
                let (val_offset, _) = self.create_call_stack_local(builder, ty, false, true);
                self.write_to_memory(builder, val_offset, 0, ty)?;

                let serialized_ty = self.type_for_serialization(ty).to_string();

                // Validate serialized type
                signature_from_string(
                    &serialized_ty,
                    self.contract_analysis.clarity_version,
                    self.contract_analysis.epoch,
                )
                .map_err(|e| {
                    GeneratorError::TypeError(format!("type cannot be deserialized: {e:?}"))
                })?;

                let (type_ser_offset, type_ser_len) =
                    self.add_clarity_string_literal(&CharType::ASCII(ASCIIData {
                        data: serialized_ty.into_bytes(),
                    }))?;

                // Set runtime error globals
                builder
                    .local_get(val_offset)
                    .global_set(get_global(&self.module, "runtime-error-value-offset")?)
                    .i32_const(type_ser_offset as i32)
                    .global_set(get_global(&self.module, "runtime-error-type-ser-offset")?)
                    .i32_const(type_ser_len as i32)
                    .global_set(get_global(&self.module, "runtime-error-type-ser-len")?)
                    .i32_const(runtime_error as i32)
                    .call(self.func_by_name("stdlib.runtime-error"));
            }
            ErrorMap::ShortReturnExpectedValueOptional => {
                // Simple case: just call runtime error
                builder
                    .i32_const(runtime_error as i32)
                    .call(self.func_by_name("stdlib.runtime-error"));
            }
            _ => {
                return Err(GeneratorError::InternalError(
                    "Unhandled runtime error for try! function".to_owned(),
                ));
            }
        }

        builder.unreachable();

        Ok(())
    }

    /// Write `expr`'s analysed type into the module's literal memory, so a host
    /// function can read a value of it back out of Wasm memory.
    ///
    /// The compiler is the only place that knows an expression's type, and some
    /// host functions need it: `print` has to serialize the value it is given,
    /// and `with_nft` has to read a list of asset identifiers whose element type
    /// no NFT definition need supply. Answers the literal's offset and length.
    ///
    /// The round trip is checked here, at compile time, so a type that cannot be
    /// reconstructed fails the build rather than the call.
    pub(crate) fn serialized_type_of(
        &mut self,
        expr: &SymbolicExpression,
    ) -> Result<(i32, i32), GeneratorError> {
        let ty = self
            .get_expr_type(expr)
            .ok_or_else(|| {
                GeneratorError::TypeError("expression must be typed to be serialized".to_owned())
            })?
            .clone();
        self.serialized_type(&ty)
    }

    pub(crate) fn serialized_type(
        &mut self,
        ty: &TypeSignature,
    ) -> Result<(i32, i32), GeneratorError> {
        let serialized = self.type_for_serialization(ty).to_string();
        signature_from_string(
            &serialized,
            self.contract_analysis.clarity_version,
            self.contract_analysis.epoch,
        )
        .map_err(|error| {
            GeneratorError::TypeError(format!("serialized type cannot be deserialized: {error:?}"))
        })?;
        let (offset, length) = self.add_clarity_string_literal(&CharType::ASCII(ASCIIData {
            data: serialized.into_bytes(),
        }))?;
        Ok((offset as i32, length as i32))
    }

    fn serialized_runtime_type(
        &mut self,
        ty: &TypeSignature,
    ) -> Result<(i32, i32), GeneratorError> {
        let serialized =
            serde_json::to_vec(ty).map_err(|error| GeneratorError::TypeError(error.to_string()))?;
        let (offset, length) =
            self.add_clarity_string_literal(&CharType::ASCII(ASCIIData { data: serialized }))?;
        Ok((offset as i32, length as i32))
    }

    /// Give a `filter` result the list capacity its input carried.
    ///
    /// The reference's `filter` mutates its argument in place and returns the
    /// same value, so the result keeps the input's `max_len` however many
    /// elements it dropped — and a list value is sized by `max_len`, not by its
    /// length. The compiler builds a fresh, compacted buffer instead, so its
    /// size came out as the *kept* count and every filter that dropped anything
    /// under-charged every later measurement of the result.
    ///
    /// Only when the two differ, which keeps the arena out of the common case:
    /// a filter that keeps everything already measures identically, and a fold
    /// that filters per iteration would otherwise materialize a value a round.
    ///
    /// Expects `[handle, offset, length]` for the result on the stack and leaves
    /// the same triple, with the handle replaced when one was taken.
    pub(crate) fn capture_filtered_runtime_shape(
        &mut self,
        builder: &mut InstrSeqBuilder,
        ty: &TypeSignature,
        input_handle: LocalId,
        input_length: LocalId,
        element_stride: i32,
    ) -> Result<(), GeneratorError> {
        let locals = self.save_to_locals(builder, ty, true);
        let handle = *locals.first().ok_or_else(|| {
            GeneratorError::InternalError("filter result is missing its shape handle".to_owned())
        })?;
        let length = *locals.last().ok_or_else(|| {
            GeneratorError::InternalError("filter result is missing its length".to_owned())
        })?;
        let (type_offset, type_length) = self.serialized_runtime_type(ty)?;

        let mut capture = builder.dangling_instr_seq(None);
        for local in &locals {
            capture.local_get(*local);
        }
        let (value_offset, _) = self.create_call_stack_local(&mut capture, ty, true, false);
        self.write_to_memory(&mut capture, value_offset, 0, ty)?;
        capture
            .local_get(value_offset)
            .i32_const(type_offset)
            .i32_const(type_length)
            .local_get(input_handle)
            .local_get(input_length)
            .i32_const(element_stride)
            .binop(BinaryOp::I32DivU)
            .call(self.func_by_name("stdlib.save_filtered_runtime_shape"))
            .local_set(handle);
        let capture = capture.id();

        // Nothing to inherit when the input was not itself widened *and* the
        // result kept every element: then the input's capacity is its length,
        // which is the result's, and the inline measurement already answers
        // what the reference answers. A widened input has to be asked even when
        // nothing was dropped, because its capacity is wider than its length.
        builder
            .local_get(input_handle)
            .local_get(input_length)
            .local_get(length)
            .binop(BinaryOp::I32Ne)
            .binop(BinaryOp::I32Or)
            .if_else(
                None,
                |then| {
                    then.instr(walrus::ir::Block { seq: capture });
                },
                |_| {},
            );

        for local in &locals {
            builder.local_get(*local);
        }
        self.release_locals(locals);
        Ok(())
    }

    /// Capture a composite that was *built* from parts, when one of those parts
    /// was itself widened.
    ///
    /// `runtime_size`'s tuple arm reads a zero handle as "nothing widened this
    /// value — widening is a preservation or host crossing, and crossings assign
    /// handles". A composite constructed out of a widened field breaks that: the
    /// constructor pushes a literal zero, the inline sum then measures the field
    /// by its run-time length, and the capacity the field carried is gone. A
    /// `print` of a tuple holding a `(list 12000 uint)` read from a map was
    /// charged 534 where the reference charged 192,534 ([[150]]).
    ///
    /// `field_handles` are the handle slots of the fields that *can* carry one,
    /// so a composite of scalars is skipped at compile time and pays nothing.
    /// The rest is one runtime test: any handle set, and the value is captured.
    pub(crate) fn capture_inherited_runtime_shape(
        &mut self,
        builder: &mut InstrSeqBuilder,
        ty: &TypeSignature,
        field_handles: &[LocalId],
    ) -> Result<(), GeneratorError> {
        let Some((first, rest)) = field_handles.split_first() else {
            return Ok(());
        };
        let locals = self.save_to_locals(builder, ty, true);
        let handle = *locals.first().ok_or_else(|| {
            GeneratorError::InternalError("composite value is missing its shape handle".to_owned())
        })?;
        let (type_offset, type_length) = self.serialized_runtime_type(ty)?;

        let mut capture = builder.dangling_instr_seq(None);
        for local in &locals {
            capture.local_get(*local);
        }
        let (value_offset, _) = self.create_call_stack_local(&mut capture, ty, true, false);
        self.write_to_memory(&mut capture, value_offset, 0, ty)?;
        capture
            .local_get(value_offset)
            .i32_const(type_offset)
            .i32_const(type_length)
            .call(self.func_by_name("stdlib.save_runtime_shape"))
            .local_set(handle);
        let capture = capture.id();

        builder.local_get(*first);
        for other in rest {
            builder.local_get(*other).binop(BinaryOp::I32Or);
        }
        builder.if_else(
            None,
            |then| {
                then.instr(walrus::ir::Block { seq: capture });
            },
            |_| {},
        );

        for local in &locals {
            builder.local_get(*local);
        }
        self.release_locals(locals);
        Ok(())
    }

    /// Materialize a composite stack value in the execution context's
    /// runtime-shape arena, then put the same projected value back on stack
    /// with its new handle.
    pub(crate) fn capture_runtime_shape(
        &mut self,
        builder: &mut InstrSeqBuilder,
        ty: &TypeSignature,
    ) -> Result<(), GeneratorError> {
        if !matches!(
            ty,
            TypeSignature::TupleType(_) | TypeSignature::SequenceType(SequenceSubtype::ListType(_))
        ) {
            return Err(GeneratorError::InternalError(
                "only tuples and lists carry runtime-shape handles".to_owned(),
            ));
        }

        let locals = self.save_to_locals(builder, ty, true);
        let handle = *locals.first().ok_or_else(|| {
            GeneratorError::InternalError("composite value is missing its shape handle".to_owned())
        })?;
        let (type_offset, type_length) = self.serialized_runtime_type(ty)?;
        let mut capture = builder.dangling_instr_seq(None);
        for local in &locals {
            capture.local_get(*local);
        }
        let (value_offset, _) = self.create_call_stack_local(&mut capture, ty, true, false);
        self.write_to_memory(&mut capture, value_offset, 0, ty)?;
        capture
            .local_get(value_offset)
            .i32_const(type_offset)
            .i32_const(type_length)
            .call(self.func_by_name("stdlib.save_runtime_shape"))
            .local_set(handle);
        let capture = capture.id();

        builder.local_get(handle).unop(UnaryOp::I32Eqz).if_else(
            None,
            |then| {
                then.instr(walrus::ir::Block { seq: capture });
            },
            |_| {},
        );
        for local in &locals {
            builder.local_get(*local);
        }
        self.release_locals(locals);
        Ok(())
    }

    /// Try to change `ty` for serialization/deserialization (as stringified signature)
    /// In case of failure, clones the input `ty`
    #[allow(clippy::only_used_in_recursion)]
    pub fn type_for_serialization(&self, ty: &TypeSignature) -> TypeSignature {
        use clarity::vm::types::signatures::TypeSignature::*;
        match ty {
            // NoType and BoolType have the same size (both type and inner)
            NoType => BoolType,
            // Callable metadata is not part of a serialized principal value.
            //
            // `TraitReferenceType` is here because it is the *same type* under
            // an older type checker: 2.05's types a `<trait>` parameter
            // `TraitReferenceType` where 2.1's types it
            // `CallableType(CallableSubtype::Trait(_))`, and a contract analysed
            // in 2.05 keeps that spelling forever. Both carry a trait identifier
            // that a serialized value does not, and both are a contract
            // principal at run time — the reference implementation prints such a
            // value with the tuple type `principal`, which is measured in
            // `words/traits.rs`'s
            // `print_a_trait_reference_under_the_two_oh_five_type_checker`.
            //
            // Leaving it out was mainnet 8,707,847: the type written into
            // literal memory came out as `<SP2PAB….nft-trait.nft-trait>`, and a
            // qualified trait identifier inside angle brackets is not Clarity —
            // `<…>` takes the local alias `use-trait` introduces — so the round
            // trip in `serialized_type_of` refused the module at a *call*.
            CallableType(_) | ListUnionType(_) | TraitReferenceType(_) => PrincipalType,
            // Recursive types
            ResponseType(types) => ResponseType(Box::new((
                self.type_for_serialization(&types.0),
                self.type_for_serialization(&types.1),
            ))),
            OptionalType(value_ty) => OptionalType(Box::new(self.type_for_serialization(value_ty))),
            SequenceType(SequenceSubtype::ListType(list_ty)) => {
                SequenceType(SequenceSubtype::ListType(
                    ListTypeData::new_list(
                        self.type_for_serialization(list_ty.get_list_item_type()),
                        list_ty.get_max_len(),
                    )
                    .unwrap_or_else(|_| list_ty.clone()),
                ))
            }
            TupleType(tuple_ty) => TupleType(
                TupleTypeSignature::try_from(
                    tuple_ty
                        .get_type_map()
                        .iter()
                        .map(|(k, v)| (k.clone(), self.type_for_serialization(v)))
                        .collect::<Vec<_>>(),
                )
                .unwrap_or_else(|_| tuple_ty.clone()),
            ),
            t => t.clone(),
        }
    }

    pub fn clarity_value_size(&self, ty: &TypeSignature) -> Result<u32, GeneratorError> {
        ty.size()
            .map_err(|error| GeneratorError::TypeError(error.to_string()))
    }

    /// Emit the size of a value already saved in `locals` into `size`.
    ///
    /// From epoch 3.3 costs are charged on the size of the value rather than
    /// of its declared type (`callables.rs`, `uses_arg_size_for_cost`), and a
    /// wrapper's declared size is its widest branch: `(optional (buff 500))`
    /// holding two bytes declares 505 and is 7. Whatever the value carries at
    /// runtime — a discriminant, a sequence length — is on the stack here, so
    /// the size follows it. What cannot be told apart at runtime keeps the
    /// declared size, which over-charges but never under.
    pub(crate) fn runtime_size(
        &mut self,
        builder: &mut InstrSeqBuilder,
        ty: &TypeSignature,
        locals: &[LocalId],
        size: LocalId,
    ) -> Result<(), GeneratorError> {
        let expected_locals = clar2wasm_ty(ty).len();
        if locals.len() != expected_locals {
            return Err(GeneratorError::InternalError(format!(
                "runtime size for {ty} needs {expected_locals} locals, got {}",
                locals.len()
            )));
        }
        match ty {
            TypeSignature::OptionalType(inner) => {
                let some = {
                    let mut some = builder.dangling_instr_seq(None);
                    self.runtime_size(&mut some, inner, &locals[1..], size)?;
                    some.local_get(size)
                        .i32_const(1)
                        .binop(BinaryOp::I32Add)
                        .local_set(size);
                    some.id()
                };

                let none = {
                    let mut none = builder.dangling_instr_seq(None);
                    none.i32_const(2).local_set(size);
                    none.id()
                };

                builder.local_get(locals[0]).instr(IfElse {
                    consequent: some,
                    alternative: none,
                });
            }
            TypeSignature::ResponseType(response) => {
                let ok_locals = clar2wasm_ty(&response.0).len();
                let err_locals = clar2wasm_ty(&response.1).len();

                let ok = {
                    let mut ok = builder.dangling_instr_seq(None);
                    self.runtime_size(&mut ok, &response.0, &locals[1..1 + ok_locals], size)?;
                    ok.local_get(size)
                        .i32_const(1)
                        .binop(BinaryOp::I32Add)
                        .local_set(size);
                    ok.id()
                };

                let err = {
                    let mut err = builder.dangling_instr_seq(None);
                    self.runtime_size(
                        &mut err,
                        &response.1,
                        &locals[1 + ok_locals..1 + ok_locals + err_locals],
                        size,
                    )?;
                    err.local_get(size)
                        .i32_const(1)
                        .binop(BinaryOp::I32Add)
                        .local_set(size);
                    err.id()
                };

                builder.local_get(locals[0]).instr(IfElse {
                    consequent: ok,
                    alternative: err,
                });
            }
            // A byte sequence is `4 + len`, and a UTF-8 string is
            // `4 + 4 * characters` over four-byte scalars, so both are four
            // more than the bytes on the stack.
            TypeSignature::SequenceType(
                SequenceSubtype::BufferType(_)
                | SequenceSubtype::StringType(StringSubtype::ASCII(_) | StringSubtype::UTF8(_)),
            ) => {
                builder
                    .local_get(locals[1])
                    .i32_const(4)
                    .binop(BinaryOp::I32Add)
                    .local_set(size);
            }
            TypeSignature::SequenceType(SequenceSubtype::ListType(list_type)) => {
                // The reference sizes a list value as
                // `len × least-supertype(element dynamic types).size() + list
                // type_size`. When the element type is its own dynamic type —
                // every `int` is an `IntType`, every principal a
                // `PrincipalType` — the supertype fold is the identity, the
                // length is the byte length over the element stride, and the
                // measurement is arithmetic over locals this function already
                // holds. A fold measures per iteration, so the host round
                // trip it replaces was quadratic in practice. Any other
                // element type, and any list a nonzero handle says was
                // widened, still asks the host, which sizes the arena value
                // the handle names.
                let element = list_type.get_list_item_type();
                let invariant_element_size = match element {
                    TypeSignature::IntType | TypeSignature::UIntType => Some(16),
                    TypeSignature::BoolType => Some(1),
                    TypeSignature::PrincipalType => Some(148),
                    _ => None,
                };
                // A handled value's measurement is one host call carrying the
                // handle: writing the whole representation into a fresh
                // region for the host to read the handle back out of it was
                // ceremony, paid per measurement.
                let handled = {
                    let mut handled = builder.dangling_instr_seq(None);
                    handled
                        .local_get(locals[0])
                        .call(self.func_by_name("stdlib.runtime_shape_size"))
                        .local_set(size);
                    handled.id()
                };
                let unhandled = if let Some(element_size) = invariant_element_size {
                    // `ListTypeData::type_size` is `entry.inner_type_size + 5`,
                    // count-independent — and 1 for every invariant element
                    // type, which is also `NoType`'s, so the empty list (whose
                    // entry type the reference derives as `NoType`) answers
                    // identically.
                    let list_type_size = TypeSignature::SequenceType(SequenceSubtype::ListType(
                        ListTypeData::new_list(element.clone(), 0).map_err(|error| {
                            GeneratorError::TypeError(format!(
                                "cannot size the empty list of {element}: {error}"
                            ))
                        })?,
                    ))
                    .type_size()
                    .map_err(|error| {
                        GeneratorError::TypeError(format!(
                            "cannot size the list type of {element}: {error}"
                        ))
                    })?;
                    let mut inline = builder.dangling_instr_seq(None);
                    inline
                        .local_get(locals[2])
                        .i32_const(get_type_size(element))
                        .binop(BinaryOp::I32DivU)
                        .i32_const(element_size)
                        .binop(BinaryOp::I32Mul)
                        .i32_const(list_type_size as i32)
                        .binop(BinaryOp::I32Add)
                        .local_set(size);
                    inline.id()
                } else {
                    let mut host = builder.dangling_instr_seq(None);
                    for local in locals {
                        host.local_get(*local);
                    }
                    let (value_offset, _) =
                        self.create_call_stack_local(&mut host, ty, true, false);
                    self.write_to_memory(&mut host, value_offset, 0, ty)?;
                    let (type_offset, type_length) = self.serialized_type(ty)?;
                    host.local_get(value_offset)
                        .i32_const(type_offset)
                        .i32_const(type_length)
                        .call(self.func_by_name("stdlib.runtime_value_size"))
                        .local_set(size);
                    host.id()
                };
                builder.local_get(locals[0]).instr(IfElse {
                    consequent: handled,
                    alternative: unhandled,
                });
            }
            TypeSignature::TupleType(tuple) => {
                // A tuple's first local is its runtime-shape handle. Zero
                // means nothing widened this value — widening is a
                // preservation or host crossing, and crossings assign handles
                // — so its size is the fixed tuple overhead plus its fields',
                // each already in locals: the sum the host computed by having
                // the whole value serialized to memory and read back. A set
                // handle keeps that host path, which sizes the arena value
                // the handle names.
                //
                // A tuple's size counts `2 * fields + names` twice, once in
                // its own right and once inside the type size it embeds. Only
                // that embedded term depends on the *dynamic* field types, so
                // it is the declared one exactly when every field's is.
                let plans: Vec<_> = tuple.get_type_map().values().map(type_size_plan).collect();
                let measured = !plans.iter().all(|plan| *plan == TypeSizePlan::Declared);
                let unmeasurable = plans.contains(&TypeSizePlan::Host);
                let names = u32::try_from(tuple.get_type_map().len())
                    .ok()
                    .and_then(|fields| fields.checked_mul(2))
                    .and_then(|base| {
                        tuple
                            .get_type_map()
                            .keys()
                            .try_fold(base, |sum, name| sum.checked_add(u32::from(name.len())))
                    })
                    .ok_or_else(|| {
                        GeneratorError::TypeError(format!("tuple overhead overflows: {ty}"))
                    })?;
                let overhead = if measured {
                    names.checked_mul(2)
                } else {
                    tuple.type_size().and_then(|ty| names.checked_add(ty))
                }
                .ok_or_else(|| {
                    GeneratorError::TypeError(format!("tuple overhead overflows: {ty}"))
                })?;
                let field_size = self.borrow_local(ValType::I32);
                // A field whose dynamic type size only the host can give makes
                // the whole tuple the host's to size, handle or not.
                let unhandled = if unmeasurable {
                    let mut materialize = builder.dangling_instr_seq(None);
                    for local in locals {
                        materialize.local_get(*local);
                    }
                    let (value_offset, _) =
                        self.create_call_stack_local(&mut materialize, ty, true, false);
                    self.write_to_memory(&mut materialize, value_offset, 0, ty)?;
                    let (type_offset, type_length) = self.serialized_type(ty)?;
                    materialize
                        .local_get(value_offset)
                        .i32_const(type_offset)
                        .i32_const(type_length)
                        .call(self.func_by_name("stdlib.runtime_value_size"))
                        .local_set(size);
                    materialize.id()
                } else {
                    let mut inline = builder.dangling_instr_seq(None);
                    inline.i32_const(overhead as i32).local_set(size);
                    let mut cursor = 1;
                    for field_ty in tuple.get_type_map().values() {
                        let width = clar2wasm_ty(field_ty).len();
                        let field_locals = &locals[cursor..cursor + width];
                        self.runtime_size(&mut inline, field_ty, field_locals, *field_size)?;
                        inline
                            .local_get(size)
                            .local_get(*field_size)
                            .binop(BinaryOp::I32Add)
                            .local_set(size);
                        if measured {
                            self.runtime_type_size(
                                &mut inline,
                                field_ty,
                                field_locals,
                                *field_size,
                            )?;
                            inline
                                .local_get(size)
                                .local_get(*field_size)
                                .binop(BinaryOp::I32Add)
                                .local_set(size);
                        }
                        cursor += width;
                    }
                    inline.id()
                };
                let handled = {
                    // The handle names the arena value the host would size
                    // anyway; passing it is the whole message.
                    let mut handled = builder.dangling_instr_seq(None);
                    handled
                        .local_get(locals[0])
                        .call(self.func_by_name("stdlib.runtime_shape_size"))
                        .local_set(size);
                    handled.id()
                };
                builder.local_get(locals[0]).instr(IfElse {
                    consequent: handled,
                    alternative: unhandled,
                });
            }
            TypeSignature::CallableType(CallableSubtype::Trait(_))
            | TypeSignature::TraitReferenceType(_) => {
                // The value behind a trait reference is a callable. From
                // Clarity 2 it carries its trait identifier and the reference
                // sizes it as a trait (276); a Clarity 1 contract's callables
                // never carry one, so the reference sizes them as bare
                // contract principals (148), and so must the charge.
                let value_size =
                    if self.contract_analysis.clarity_version >= ClarityVersion::Clarity2 {
                        276
                    } else {
                        148
                    };
                builder.i32_const(value_size).local_set(size);
            }
            _ => {
                builder
                    .i32_const(self.clarity_value_size(ty)? as i32)
                    .local_set(size);
            }
        }
        Ok(())
    }

    /// Measure a value's dynamic type size into `out`.
    ///
    /// Only reached for types [`type_size_plan`] admits, so the composite arms
    /// are constants and the recursion never reads through a shape handle.
    fn runtime_type_size(
        &mut self,
        builder: &mut InstrSeqBuilder,
        ty: &TypeSignature,
        locals: &[LocalId],
        out: LocalId,
    ) -> Result<(), GeneratorError> {
        // `(optional t)` is `t + 1`, and `none` is `(optional NoType)`; a
        // response is `ok + err + 1` with `NoType` standing in for the arm the
        // value did not take, so either arm is its own size plus two.
        match ty {
            TypeSignature::OptionalType(inner) => {
                let some = {
                    let mut some = builder.dangling_instr_seq(None);
                    self.runtime_type_size(&mut some, inner, &locals[1..], out)?;
                    some.local_get(out)
                        .i32_const(1)
                        .binop(BinaryOp::I32Add)
                        .local_set(out);
                    some.id()
                };
                let none = {
                    let mut none = builder.dangling_instr_seq(None);
                    none.i32_const(2).local_set(out);
                    none.id()
                };
                builder.local_get(locals[0]).instr(IfElse {
                    consequent: some,
                    alternative: none,
                });
            }
            TypeSignature::ResponseType(response) => {
                let ok_locals = clar2wasm_ty(&response.0).len();
                let err_locals = clar2wasm_ty(&response.1).len();
                let ok = {
                    let mut ok = builder.dangling_instr_seq(None);
                    self.runtime_type_size(&mut ok, &response.0, &locals[1..=ok_locals], out)?;
                    ok.local_get(out)
                        .i32_const(2)
                        .binop(BinaryOp::I32Add)
                        .local_set(out);
                    ok.id()
                };
                let err = {
                    let mut err = builder.dangling_instr_seq(None);
                    self.runtime_type_size(
                        &mut err,
                        &response.1,
                        &locals[1 + ok_locals..1 + ok_locals + err_locals],
                        out,
                    )?;
                    err.local_get(out)
                        .i32_const(2)
                        .binop(BinaryOp::I32Add)
                        .local_set(out);
                    err.id()
                };
                builder.local_get(locals[0]).instr(IfElse {
                    consequent: ok,
                    alternative: err,
                });
            }
            _ => {
                let declared = ty.type_size().map_err(|error| {
                    GeneratorError::TypeError(format!("cannot size the type of {ty}: {error}"))
                })?;
                builder.i32_const(declared as i32).local_set(out);
            }
        }
        Ok(())
    }

    pub fn clarity_value_size_on_stack(
        &mut self,
        builder: &mut InstrSeqBuilder,
        ty: &TypeSignature,
    ) -> Result<(), GeneratorError> {
        let values = self.save_to_locals(builder, ty, true);
        let size = self.borrow_local(ValType::I32);
        self.runtime_size(builder, ty, &values, *size)?;

        for value in &values {
            builder.local_get(*value);
        }
        builder.local_get(*size);

        // The saved values are back on the stack; their slots are dead.
        self.release_locals(values);

        Ok(())
    }

    /// Gets the result type of the given `SymbolicExpression`.
    pub fn get_expr_type(&self, expr: &SymbolicExpression) -> Option<&TypeSignature> {
        if let Some(ty) = self.lowered_type_overrides.get(&expr.id) {
            return Some(ty);
        }
        self.get_source_expr_type(expr)
    }

    /// Gets the analyser's original result type without compiler-only layout
    /// refinements.
    pub(crate) fn get_source_expr_type(&self, expr: &SymbolicExpression) -> Option<&TypeSignature> {
        self.contract_analysis
            .type_map
            .as_ref()
            .and_then(|ty| ty.get_type_expected(expr))
    }

    /// Sets the result type of the given `SymbolicExpression`. This is
    /// necessary to overcome some weaknesses in the type-checker and
    /// hopefully can be removed in the future.
    pub fn set_expr_type(
        &mut self,
        expr: &SymbolicExpression,
        ty: TypeSignature,
    ) -> Result<(), GeneratorError> {
        if self.contract_analysis.type_map.is_none() {
            return Err(GeneratorError::InternalError(
                "type-checker must be called before Wasm generation".to_owned(),
            ));
        }
        self.lowered_type_overrides.insert(expr.id, ty);
        Ok(())
    }

    /// Adds a new string literal into the memory, and returns the offset and length.
    pub(crate) fn add_clarity_string_literal(
        &mut self,
        s: &CharType,
    ) -> Result<(u32, u32), GeneratorError> {
        // If this string has already been saved in the literal memory,
        // just return the offset and length.
        let (data, entry) = match s {
            CharType::ASCII(s) => {
                let entry = LiteralMemoryEntry::Ascii(s.to_string());
                if let Some(offset) = self.literal_memory_offset.get(&entry) {
                    return Ok((*offset, s.data.len() as u32));
                }
                (s.data.clone(), entry)
            }
            CharType::UTF8(u) => {
                let data_str = String::from_utf8(u.data.iter().flatten().cloned().collect())
                    .map_err(|_e| {
                        GeneratorError::InternalError("Invalid UTF-8 sequence".to_owned())
                    })?;
                let entry = LiteralMemoryEntry::Utf8(data_str.clone());
                if let Some(offset) = self.literal_memory_offset.get(&entry) {
                    return Ok((*offset, u.data.len() as u32 * 4));
                }
                // Convert the string into 4-byte big-endian unicode scalar values.
                let data = data_str
                    .chars()
                    .flat_map(|c| (c as u32).to_be_bytes())
                    .collect();
                (data, entry)
            }
        };
        let memory = self.get_memory()?;
        let offset = self.literal_memory_end;
        let len = data.len() as u32;
        self.module.data.add(
            DataKind::Active(ActiveData {
                memory,
                location: walrus::ActiveDataLocation::Absolute(offset),
            }),
            data,
        );
        self.literal_memory_end += len;

        // Save the offset in the literal memory for this string
        self.literal_memory_offset.insert(entry, offset);

        Ok((offset, len))
    }

    /// Adds a new string literal into the memory for an identifier
    pub(crate) fn add_string_literal(&mut self, name: &str) -> Result<(u32, u32), GeneratorError> {
        // If this identifier has already been saved in the literal memory,
        // just return the offset and length.
        let entry = LiteralMemoryEntry::Ascii(name.to_string());
        if let Some(offset) = self.literal_memory_offset.get(&entry) {
            return Ok((*offset, name.len() as u32));
        }

        let memory = self.get_memory()?;
        let offset = self.literal_memory_end;
        let len = name.len() as u32;
        self.module.data.add(
            DataKind::Active(ActiveData {
                memory,
                location: walrus::ActiveDataLocation::Absolute(offset),
            }),
            name.as_bytes().to_vec(),
        );
        self.literal_memory_end += name.len() as u32;

        // Save the offset in the literal memory for this identifier
        self.literal_memory_offset.insert(entry, offset);

        Ok((offset, len))
    }

    pub(crate) fn add_bytes_literal(&mut self, bytes: &[u8]) -> Result<(u32, u32), GeneratorError> {
        let entry = LiteralMemoryEntry::Bytes(bytes.into());
        if let Some(offset) = self.literal_memory_offset.get(&entry) {
            return Ok((*offset, bytes.len() as u32));
        }

        let memory = self.get_memory()?;
        let offset = self.literal_memory_end;
        let len = bytes.len() as u32;
        self.module.data.add(
            DataKind::Active(ActiveData {
                memory,
                location: walrus::ActiveDataLocation::Absolute(offset),
            }),
            bytes.to_vec(),
        );
        self.literal_memory_end += len;

        self.literal_memory_offset.insert(entry, offset);

        Ok((offset, len))
    }

    pub(crate) fn reserve_static_memory(&mut self, size: u32) -> u32 {
        let offset = self.literal_memory_end;
        self.literal_memory_end += size;
        offset
    }

    /// Adds a serialized [TraitIdentifier] to the wasm memory.
    /// Returns the offset and length of the bytes written.
    pub(crate) fn add_trait_identifier(
        &mut self,
        trait_id: &TraitIdentifier,
    ) -> Result<(u32, u32), GeneratorError> {
        self.add_bytes_literal(&trait_identifier_as_bytes(trait_id))
    }

    /// Adds a new literal into the memory, and returns the offset and length.
    pub(crate) fn add_literal(
        &mut self,
        value: &clarity::vm::Value,
    ) -> Result<(u32, u32), GeneratorError> {
        let data = match value {
            clarity::vm::Value::Int(i) => {
                let mut data = (((*i as u128) & 0xFFFFFFFFFFFFFFFF) as i64)
                    .to_le_bytes()
                    .to_vec();
                data.extend_from_slice(&(((*i as u128) >> 64) as i64).to_le_bytes());
                data
            }
            clarity::vm::Value::UInt(u) => {
                let mut data = ((*u & 0xFFFFFFFFFFFFFFFF) as i64).to_le_bytes().to_vec();
                data.extend_from_slice(&((*u >> 64) as i64).to_le_bytes());
                data
            }
            clarity::vm::Value::Principal(p) => match p {
                PrincipalData::Standard(standard) => {
                    let mut data = vec![standard.version()];
                    data.extend_from_slice(&standard.1);
                    // Append a 0 for the length of the contract name
                    data.push(0);
                    data
                }
                PrincipalData::Contract(contract) => {
                    let mut data = vec![contract.issuer.version()];
                    data.extend_from_slice(&contract.issuer.1);
                    let contract_length = contract.name.len();
                    data.push(contract_length);
                    data.extend_from_slice(contract.name.as_bytes());
                    data
                }
            },
            clarity::vm::Value::Sequence(SequenceData::Buffer(buff_data)) => buff_data.data.clone(),
            clarity::vm::Value::Sequence(SequenceData::String(string_data)) => {
                return self.add_clarity_string_literal(string_data);
            }
            clarity::vm::Value::Bool(_)
            | clarity::vm::Value::Tuple(_)
            | clarity::vm::Value::Optional(_)
            | clarity::vm::Value::Response(_)
            | clarity::vm::Value::CallableContract(_)
            | clarity::vm::Value::Sequence(_) => {
                return Err(GeneratorError::TypeError(format!(
                    "Not a valid literal type: {value:?}"
                )));
            }
        };
        let memory = self.get_memory()?;
        let offset = self.literal_memory_end;
        let len = data.len() as u32;
        self.module.data.add(
            DataKind::Active(ActiveData {
                memory,
                location: walrus::ActiveDataLocation::Absolute(offset),
            }),
            data.clone(),
        );
        self.literal_memory_end += data.len() as u32;

        Ok((offset, len))
    }

    pub(crate) fn block_from_expr(
        &mut self,
        builder: &mut InstrSeqBuilder,
        expr: &SymbolicExpression,
    ) -> Result<InstrSeqId, GeneratorError> {
        let return_type = clar2wasm_ty(self.get_expr_type(expr).ok_or_else(|| {
            GeneratorError::TypeError("Expression results must be typed".to_owned())
        })?);

        let source_results = self
            .get_source_expr_type(expr)
            .map_or(return_type.len(), source_wasm_arity);
        self.note_control_arity(0, source_results);
        let block_type = self.checked_control_type(&[], &return_type)?;
        let mut block = builder.dangling_instr_seq(block_type);
        self.traverse_expr(&mut block, expr)?;

        Ok(block.id())
    }

    /// Record a control signature and construct it only when Wasm can encode it.
    /// Wide callers must carry their value through memory and use an empty type.
    pub(crate) fn bounded_control_type(
        &mut self,
        params: &[ValType],
        results: &[ValType],
    ) -> Result<InstrSeqType, GeneratorError> {
        self.note_control_arity(params.len(), results.len());
        self.checked_control_type(params, results)
    }

    fn checked_control_type(
        &mut self,
        params: &[ValType],
        results: &[ValType],
    ) -> Result<InstrSeqType, GeneratorError> {
        if uses_packed_slots(params.len(), results.len()) {
            return Err(GeneratorError::InternalError(format!(
                "a {}/{}-slot control value was not lowered through memory",
                params.len(),
                results.len()
            )));
        }
        Ok(InstrSeqType::new(&mut self.module.types, params, results))
    }

    /// Record the source signature for a value deliberately lowered through
    /// locals or memory, and return an empty Wasm control signature.
    pub(crate) fn lowered_control_type(
        &self,
        params: &[ValType],
        results: &[ValType],
    ) -> InstrSeqType {
        self.note_control_arity(params.len(), results.len());
        None.into()
    }

    pub(crate) fn note_control_arity(&self, params: usize, results: usize) {
        let mut report = self.arity_report.borrow_mut();
        report.max_control_params = report.max_control_params.max(params);
        report.max_control_results = report.max_control_results.max(results);
    }

    /// Build an empty control arm that evaluates `expr` into `result_offset`.
    pub(crate) fn block_from_expr_into_memory(
        &mut self,
        builder: &mut InstrSeqBuilder,
        expr: &SymbolicExpression,
        result_offset: LocalId,
        result_type: &TypeSignature,
    ) -> Result<InstrSeqId, GeneratorError> {
        self.note_control_arity(0, source_wasm_arity(result_type));
        let mut block = builder.dangling_instr_seq(None);
        self.traverse_expr(&mut block, expr)?;
        self.write_to_memory(&mut block, result_offset, 0, result_type)?;
        Ok(block.id())
    }

    pub(crate) fn create_call_stack_bytes(
        &mut self,
        builder: &mut InstrSeqBuilder,
        size: i32,
    ) -> (LocalId, i32) {
        // Save the offset (current stack pointer) into a local
        let offset = self.alloc_local(ValType::I32);
        builder
            // []
            .global_get(self.stack_pointer)
            // [ stack_ptr ]
            .local_tee(offset);
        // [ stack_ptr ]

        // TODO: The frame stack size can be computed at compile time, so we
        //       should be able to increment the stack pointer once in the function
        //       prelude with a constant instead of incrementing it for each local.
        // (global.set $stack-pointer (i32.add (global.get $stack-pointer) (i32.const <size>))
        builder
            // [ stack_ptr ]
            .i32_const(size)
            // [ stack_ptr, size ]
            .binop(BinaryOp::I32Add)
            // [ new_stack_ptr ]
            .global_set(self.stack_pointer);
        // [  ]
        self.frame_size += size;

        (offset, size)
    }

    /// Push a new local onto the call stack, adjusting the stack pointer and
    /// tracking the current function's frame size accordingly.
    /// - `include_repr` indicates if space should be reserved for the
    ///   representation of the value (e.g. the offset, length for an in-memory
    ///   type)
    /// - `include_value` indicates if space should be reserved for the value
    ///
    /// Returns a local which is a pointer to the beginning of the allocated
    /// stack space and the size of the allocated space.
    pub(crate) fn create_call_stack_local(
        &mut self,
        builder: &mut InstrSeqBuilder,
        ty: &TypeSignature,
        include_repr: bool,
        include_value: bool,
    ) -> (LocalId, i32) {
        let size = match (include_value, include_repr) {
            (true, true) => get_type_in_memory_size(ty, include_repr) + get_type_size(ty),
            (true, false) => get_type_in_memory_size(ty, include_repr),
            (false, true) => get_type_size(ty),
            (false, false) => unreachable!("must include either repr or value"),
        };
        self.create_call_stack_bytes(builder, size)
    }

    pub fn borrow_local(&mut self, ty: ValType) -> BorrowedLocal {
        let id = self.alloc_local(ty);
        BorrowedLocal {
            id,
            ty,
            pool: self.local_pool.clone(),
        }
    }

    /// Allocate a local of type `ty`, reusing a released local from the pool
    /// when one of the same type is available.
    pub(crate) fn alloc_local(&mut self, ty: ValType) -> LocalId {
        let reuse = (*self.local_pool).borrow_mut().take(ty);
        let local = match reuse {
            Some(local) => local,
            None => {
                let local = self.module.locals.add(ty);
                (*self.local_pool).borrow_mut().add(local);
                local
            }
        };
        if let Some(scope) = self.expression_locals.last_mut() {
            scope.push(local);
        }
        local
    }

    /// Return `locals` previously obtained from `save_to_locals` or
    /// `alloc_local` to the pool, so later allocations can reuse their slots.
    /// Must only be called after the last possible read of the values they
    /// hold: lexically, a binding's locals are unreadable once its scope has
    /// closed, and a pooled local is only handed out again once released.
    pub(crate) fn release_locals(&mut self, locals: Vec<LocalId>) {
        let mut pool = (*self.local_pool).borrow_mut();
        for local in locals {
            let ty = self.module.locals.get(local).ty();
            pool.give_back(ty, local);
        }
    }

    /// The binding id the use pre-pass assigned to a binding-name
    /// expression, if it introduces a `let`/`match` binding.
    pub(crate) fn binding_id(&self, name_expr: &SymbolicExpression) -> Option<u32> {
        self.binding_ids.get(&name_expr.id).copied()
    }

    /// Capture a newly evaluated lexical binding without allowing its
    /// flattened representation to consume the function's locals budget.
    pub(crate) fn capture_binding_value(
        &mut self,
        builder: &mut InstrSeqBuilder,
        name_expr: &SymbolicExpression,
        ty: &TypeSignature,
    ) -> Result<(BindingStorage, Option<u32>), GeneratorError> {
        let binding = self.binding_id(name_expr);
        if binding.is_some_and(|id| self.binding_uses[id as usize] == 0) {
            drop_value(builder, ty);
            return Ok((BindingStorage::Locals(Vec::new()), binding));
        }
        if self.spilled_bindings.contains(&name_expr.id) {
            let base = self.frame_pointer.ok_or_else(|| {
                GeneratorError::InternalError(
                    "spilled binding written outside of its frame".to_owned(),
                )
            })?;
            let delta = self.spill_cursor;
            let bytes = u32::try_from(get_type_size(ty)).map_err(|_| {
                GeneratorError::InternalError("negative binding representation size".to_owned())
            })?;
            self.spill_cursor = self.spill_cursor.checked_add(bytes).ok_or_else(|| {
                GeneratorError::InternalError("binding spill offset overflow".to_owned())
            })?;
            self.write_to_memory(builder, base, delta, ty)?;
            return Ok((BindingStorage::Memory { base, delta }, binding));
        }
        Ok((
            BindingStorage::Locals(self.save_to_locals(builder, ty, true)),
            binding,
        ))
    }

    /// Note a read of a binding's locals, returning them to the pool when it
    /// was the binding's last read: the pre-pass counted every read, and
    /// code generation traverses each expression exactly once. Parameters
    /// carry no binding id and stay live for their whole body.
    fn note_binding_read(&mut self, binding: Option<u32>, locals: &[LocalId]) {
        let Some(id) = binding else { return };
        let Some(remaining) = self.binding_uses.get_mut(id as usize) else {
            return;
        };
        // A count already at zero means the expression was traversed more
        // times than the pre-pass counted reads; leave the slots alone.
        if *remaining == 0 {
            return;
        }
        *remaining -= 1;
        if *remaining == 0 {
            self.release_locals(locals.to_vec());
        }
    }

    /// Write the value that is on the top of the data stack, which has type
    /// `ty`, to the memory, at offset stored in local variable,
    /// `offset_local`, plus constant offset `offset`. Returns the number of
    /// bytes written.
    pub(crate) fn write_to_memory(
        &mut self,
        builder: &mut InstrSeqBuilder,
        offset_local: LocalId,
        offset: u32,
        ty: &TypeSignature,
    ) -> Result<u32, GeneratorError> {
        let memory = self.get_memory()?;
        match ty {
            TypeSignature::IntType | TypeSignature::UIntType => {
                // Data stack: TOP | High | Low | ...
                // Save the high/low to locals.
                let high = self.borrow_local(ValType::I64);
                let low = self.borrow_local(ValType::I64);
                builder.local_set(*high).local_set(*low);

                // Store the high/low to memory.
                builder.local_get(offset_local).local_get(*low).store(
                    memory,
                    StoreKind::I64 { atomic: false },
                    MemArg { align: 8, offset },
                );
                builder.local_get(offset_local).local_get(*high).store(
                    memory,
                    StoreKind::I64 { atomic: false },
                    MemArg {
                        align: 8,
                        offset: offset + 8,
                    },
                );
                Ok(16)
            }
            TypeSignature::SequenceType(SequenceSubtype::ListType(_)) => {
                // Data stack: TOP | Length | Offset | ShapeHandle | ...
                let seq_length = self.borrow_local(ValType::I32);
                let seq_offset = self.borrow_local(ValType::I32);
                let shape_handle = self.borrow_local(ValType::I32);
                builder
                    .local_set(*seq_length)
                    .local_set(*seq_offset)
                    .local_set(*shape_handle);

                for (value, delta) in [(*shape_handle, 0), (*seq_offset, 4), (*seq_length, 8)] {
                    builder.local_get(offset_local).local_get(value).store(
                        memory,
                        StoreKind::I32 { atomic: false },
                        MemArg {
                            align: 4,
                            offset: offset + delta,
                        },
                    );
                }
                Ok(12)
            }
            TypeSignature::PrincipalType
            | TypeSignature::CallableType(_)
            | TypeSignature::ListUnionType(_)
            | TypeSignature::TraitReferenceType(_)
            | TypeSignature::SequenceType(_) => {
                // Data stack: TOP | Length | Offset | ...
                // Save the offset/length to locals.
                let seq_offset = self.borrow_local(ValType::I32);
                let seq_length = self.borrow_local(ValType::I32);
                builder.local_set(*seq_length).local_set(*seq_offset);

                // Store the offset/length to memory.
                builder
                    .local_get(offset_local)
                    .local_get(*seq_offset)
                    .store(
                        memory,
                        StoreKind::I32 { atomic: false },
                        MemArg { align: 4, offset },
                    );
                builder
                    .local_get(offset_local)
                    .local_get(*seq_length)
                    .store(
                        memory,
                        StoreKind::I32 { atomic: false },
                        MemArg {
                            align: 4,
                            offset: offset + 4,
                        },
                    );
                Ok(8)
            }
            TypeSignature::BoolType => {
                // Data stack: TOP | Value | ...
                // Save the value to a local.
                let bool_val = self.borrow_local(ValType::I32);
                builder.local_set(*bool_val);

                // Store the value to memory.
                builder.local_get(offset_local).local_get(*bool_val).store(
                    memory,
                    StoreKind::I32 { atomic: false },
                    MemArg { align: 4, offset },
                );
                Ok(4)
            }
            TypeSignature::NoType => {
                // Data stack: TOP | (Place holder i32)
                // We just have to drop the placeholder and write a i32
                builder.drop().local_get(offset_local).i32_const(0).store(
                    memory,
                    StoreKind::I32 { atomic: false },
                    MemArg { align: 4, offset },
                );
                Ok(4)
            }
            TypeSignature::OptionalType(some_ty) => {
                // Data stack: TOP | inner value | (some|none) variant
                // recursively store the inner value

                let bytes_written =
                    self.write_to_memory(builder, offset_local, offset + 4, some_ty)?;

                // Save the variant to a local and store it to memory
                let variant_val = self.borrow_local(ValType::I32);
                builder
                    .local_set(*variant_val)
                    .local_get(offset_local)
                    .local_get(*variant_val)
                    .store(
                        memory,
                        StoreKind::I32 { atomic: false },
                        MemArg { align: 4, offset },
                    );

                // recursively store the inner value
                Ok(4 + bytes_written)
            }
            TypeSignature::ResponseType(ok_err_ty) => {
                // Data stack: TOP | err_value | ok_value | (ok|err) variant
                let mut bytes_written = 0;

                // write err value at offset + size of variant (4) + size of ok_value
                bytes_written += self.write_to_memory(
                    builder,
                    offset_local,
                    offset + 4 + get_type_size(&ok_err_ty.0) as u32,
                    &ok_err_ty.1,
                )?;

                // write ok value at offset + size of variant (4)
                bytes_written +=
                    self.write_to_memory(builder, offset_local, offset + 4, &ok_err_ty.0)?;

                let variant_val = self.borrow_local(ValType::I32);
                builder
                    .local_set(*variant_val)
                    .local_get(offset_local)
                    .local_get(*variant_val)
                    .store(
                        memory,
                        StoreKind::I32 { atomic: false },
                        MemArg { align: 4, offset },
                    );

                Ok(bytes_written + 4)
            }
            TypeSignature::TupleType(tuple_ty) => {
                // Data stack: TOP | last_value | ... | first_value | ShapeHandle
                // we will write the values from last to first by setting the correct offset at which it's supposed to be written
                let mut bytes_written = 4;
                let types: Vec<_> = tuple_ty.get_type_map().values().cloned().collect();
                let offsets_delta: Vec<_> = std::iter::once(4u32)
                    .chain(
                        types
                            .iter()
                            .map(|t| get_type_size(t) as u32)
                            .scan(4, |acc, i| {
                                *acc += i;
                                Some(*acc)
                            }),
                    )
                    .collect();
                for (elem_ty, offset_delta) in types.into_iter().zip(offsets_delta).rev() {
                    bytes_written += self.write_to_memory(
                        builder,
                        offset_local,
                        offset + offset_delta,
                        &elem_ty,
                    )?;
                }
                let shape_handle = self.borrow_local(ValType::I32);
                builder
                    .local_set(*shape_handle)
                    .local_get(offset_local)
                    .local_get(*shape_handle)
                    .store(
                        memory,
                        StoreKind::I32 { atomic: false },
                        MemArg { align: 4, offset },
                    );
                Ok(bytes_written)
            }
        }
    }

    /// Read a value from memory at offset stored in local variable `offset`,
    /// with type `ty`, and push it onto the top of the data stack.
    pub(crate) fn read_from_memory(
        &mut self,
        builder: &mut InstrSeqBuilder,
        offset: LocalId,
        literal_offset: u32,
        ty: &TypeSignature,
    ) -> Result<i32, GeneratorError> {
        let memory = self
            .module
            .memories
            .iter()
            .next()
            .ok_or_else(|| GeneratorError::InternalError("No memory found".to_owned()))?;
        match ty {
            TypeSignature::IntType | TypeSignature::UIntType => {
                // Memory: Offset -> | Low | High |
                builder.local_get(offset).load(
                    memory.id(),
                    LoadKind::I64 { atomic: false },
                    MemArg {
                        align: 8,
                        offset: literal_offset,
                    },
                );
                builder.local_get(offset).load(
                    memory.id(),
                    LoadKind::I64 { atomic: false },
                    MemArg {
                        align: 8,
                        offset: literal_offset + 8,
                    },
                );
                Ok(16)
            }
            TypeSignature::OptionalType(inner) => {
                // Memory: Offset -> | Indicator | Value |
                builder.local_get(offset).load(
                    memory.id(),
                    LoadKind::I32 { atomic: false },
                    MemArg {
                        align: 4,
                        offset: literal_offset,
                    },
                );
                Ok(4 + self.read_from_memory(builder, offset, literal_offset + 4, inner)?)
            }
            TypeSignature::ResponseType(inner) => {
                // Memory: Offset -> | Indicator | Ok Value | Err Value |
                builder.local_get(offset).load(
                    memory.id(),
                    LoadKind::I32 { atomic: false },
                    MemArg {
                        align: 4,
                        offset: literal_offset,
                    },
                );
                let mut offset_adjust = 4;
                offset_adjust += self.read_from_memory(
                    builder,
                    offset,
                    literal_offset + offset_adjust,
                    &inner.0,
                )? as u32;
                offset_adjust += self.read_from_memory(
                    builder,
                    offset,
                    literal_offset + offset_adjust,
                    &inner.1,
                )? as u32;
                Ok(offset_adjust as i32)
            }
            // Principals and sequence types are stored in-memory and
            // represented by an offset and length.
            TypeSignature::SequenceType(SequenceSubtype::ListType(_)) => {
                for delta in [0, 4, 8] {
                    builder.local_get(offset).load(
                        memory.id(),
                        LoadKind::I32 { atomic: false },
                        MemArg {
                            align: 4,
                            offset: literal_offset + delta,
                        },
                    );
                }
                Ok(12)
            }
            TypeSignature::PrincipalType
            | TypeSignature::CallableType(_)
            | TypeSignature::ListUnionType(_)
            | TypeSignature::TraitReferenceType(_)
            | TypeSignature::SequenceType(_) => {
                // Memory: Offset -> | ValueOffset | ValueLength |
                builder.local_get(offset).load(
                    memory.id(),
                    LoadKind::I32 { atomic: false },
                    MemArg {
                        align: 4,
                        offset: literal_offset,
                    },
                );
                builder.local_get(offset).load(
                    memory.id(),
                    LoadKind::I32 { atomic: false },
                    MemArg {
                        align: 4,
                        offset: literal_offset + 4,
                    },
                );
                Ok(8)
            }
            TypeSignature::TupleType(tuple) => {
                // Memory: Offset -> | ShapeHandle | Value1 | Value2 | ... |
                builder.local_get(offset).load(
                    memory.id(),
                    LoadKind::I32 { atomic: false },
                    MemArg {
                        align: 4,
                        offset: literal_offset,
                    },
                );
                let mut offset_adjust = 4;
                for ty in tuple.get_type_map().values() {
                    offset_adjust +=
                        self.read_from_memory(builder, offset, literal_offset + offset_adjust, ty)?
                            as u32;
                }
                Ok(offset_adjust as i32)
            }
            // Unknown types just get a placeholder i32 value.
            TypeSignature::NoType => {
                builder.i32_const(0);
                Ok(4)
            }
            TypeSignature::BoolType => {
                builder.local_get(offset).load(
                    memory.id(),
                    LoadKind::I32 { atomic: false },
                    MemArg {
                        align: 4,
                        offset: literal_offset,
                    },
                );
                Ok(4)
            }
        }
    }

    pub(crate) fn traverse_statement_list(
        &mut self,
        builder: &mut InstrSeqBuilder,
        statements: &[SymbolicExpression],
    ) -> Result<(), GeneratorError> {
        if statements.is_empty() {
            return Err(GeneratorError::InternalError(
                "statement list must have at least one statement".to_owned(),
            ));
        }

        let mut last_ty = None;
        // Traverse the statements, saving the last non-none value.
        for stmt in statements {
            // If stmt has a type, save that type. If there was a previous type
            // saved, then drop that value.
            if let Some(ty) = self.get_expr_type(stmt) {
                if let Some(last_ty) = &last_ty {
                    drop_value(builder, last_ty);
                }
                last_ty = Some(ty.clone());
            }
            self.traverse_expr(builder, stmt)?;
        }

        Ok(())
    }

    /// If `name` is a reserved variable, push its value onto the data stack.
    pub fn lookup_reserved_variable(
        &mut self,
        builder: &mut InstrSeqBuilder,
        name: &str,
        expr: &SymbolicExpression,
    ) -> Result<bool, GeneratorError> {
        if let Some(variable) = NativeVariables::lookup_by_name_at_version(
            name,
            &self.contract_analysis.clarity_version,
        ) {
            match variable {
                NativeVariables::TxSender => {
                    // Create a new local to hold the result on the call stack
                    let (offset, size) = self.create_call_stack_local(
                        builder,
                        &TypeSignature::PrincipalType,
                        false,
                        true,
                    );

                    // Push the offset and size to the data stack
                    builder.local_get(offset).i32_const(size);

                    builder.call(self.func_by_name("stdlib.tx_sender"));

                    Ok(true)
                }
                NativeVariables::ContractCaller => {
                    // Create a new local to hold the result on the call stack
                    let (offset, size) = self.create_call_stack_local(
                        builder,
                        &TypeSignature::PrincipalType,
                        false,
                        true,
                    );

                    // Push the offset and size to the data stack
                    builder.local_get(offset).i32_const(size);

                    // Call the host interface function, `contract_caller`
                    builder.call(self.func_by_name("stdlib.contract_caller"));
                    Ok(true)
                }
                NativeVariables::TxSponsor => {
                    // Create a new local to hold the result on the call stack
                    let (offset, size) = self.create_call_stack_local(
                        builder,
                        &TypeSignature::PrincipalType,
                        false,
                        true,
                    );

                    // Push the offset and size to the data stack
                    builder.local_get(offset).i32_const(size);

                    // Call the host interface function, `tx_sponsor`

                    builder.call(self.func_by_name("stdlib.tx_sponsor"));
                    Ok(true)
                }
                NativeVariables::BlockHeight => {
                    self.charge_reserved_variable_fetch(builder)?;
                    // Call the host interface function, `block_height`
                    builder.call(self.func_by_name("stdlib.block_height"));
                    Ok(true)
                }
                NativeVariables::StacksBlockHeight => {
                    self.charge_reserved_variable_fetch(builder)?;
                    // Call the host interface function, `stacks_block_height`
                    builder.call(self.func_by_name("stdlib.stacks_block_height"));
                    Ok(true)
                }
                NativeVariables::TenureHeight => {
                    self.charge_reserved_variable_fetch(builder)?;
                    // Call the host interface function, `tenure_height`
                    builder.call(self.func_by_name("stdlib.tenure_height"));
                    Ok(true)
                }
                NativeVariables::BurnBlockHeight => {
                    self.charge_reserved_variable_fetch(builder)?;
                    // Call the host interface function, `burn_block_height`
                    builder.call(self.func_by_name("stdlib.burn_block_height"));
                    Ok(true)
                }
                NativeVariables::NativeNone => {
                    let ty = self.get_expr_type(expr).ok_or_else(|| {
                        GeneratorError::TypeError("'none' must be typed".to_owned())
                    })?;
                    add_placeholder_for_clarity_type(builder, ty);
                    Ok(true)
                }
                NativeVariables::NativeTrue => {
                    builder.i32_const(1);
                    Ok(true)
                }
                NativeVariables::NativeFalse => {
                    builder.i32_const(0);
                    Ok(true)
                }
                NativeVariables::TotalLiquidMicroSTX => {
                    self.charge_reserved_variable_fetch(builder)?;
                    // Call the host interface function, `stx_liquid_supply`
                    builder.call(self.func_by_name("stdlib.stx_liquid_supply"));
                    Ok(true)
                }
                NativeVariables::Regtest => {
                    // Call the host interface function, `is_in_regtest`
                    builder.call(self.func_by_name("stdlib.is_in_regtest"));
                    Ok(true)
                }
                NativeVariables::Mainnet => {
                    // Call the host interface function, `is_in_mainnet`
                    builder.call(self.func_by_name("stdlib.is_in_mainnet"));
                    Ok(true)
                }
                NativeVariables::ChainId => {
                    // Call the host interface function, `chain_id`
                    builder.call(self.func_by_name("stdlib.chain_id"));
                    Ok(true)
                }
                NativeVariables::StacksBlockTime => {
                    self.charge_reserved_variable_fetch(builder)?;
                    // Call the host interface function, `stacks_block_time`
                    builder.call(self.func_by_name("stdlib.stacks_block_time"));
                    Ok(true)
                }
                NativeVariables::CurrentContract => {
                    // Create a new local to hold the result on the call stack
                    let (offset, size) = self.create_call_stack_local(
                        builder,
                        &TypeSignature::PrincipalType,
                        false,
                        true,
                    );

                    // Push the offset and size to the data stack
                    builder.local_get(offset).i32_const(size);

                    // Call the host interface function, `current_contract`
                    builder.call(self.func_by_name("stdlib.current_contract"));
                    Ok(true)
                }
            }
        } else {
            Ok(false)
        }
    }

    /// If `name` is a constant, push its value onto the data stack.
    pub fn lookup_constant_variable(
        &mut self,
        builder: &mut InstrSeqBuilder,
        name: &str,
        expr: &SymbolicExpression,
    ) -> Result<bool, GeneratorError> {
        if let Some(cst_ty) = self.constants.get(name).cloned() {
            let expected_ty = self
                .get_expr_type(expr)
                .ok_or_else(|| {
                    GeneratorError::TypeError("expression using constant must be typed".to_owned())
                })?
                .clone();

            let name_offset = *self
                .literal_memory_offset
                .get(&LiteralMemoryEntry::Ascii(name.to_owned()))
                .ok_or_else(|| {
                    GeneratorError::InternalError(format!(
                        "Trying to access unsaved constant '{name}'"
                    ))
                })?;

            // Pushing the constant name and length on the stack
            builder
                .i32_const(name_offset as i32)
                .i32_const(name.len() as i32);

            if !need_ducktyping(&cst_ty, &expected_ty) {
                // if we don't need ducktyping, we can just load the constant as it is stored in db
                let (result_local, result_size) =
                    self.create_call_stack_local(builder, &cst_ty, true, true);

                builder
                    .local_get(result_local)
                    .i32_const(result_size)
                    .call(self.func_by_name("stdlib.load_constant"));

                self.read_from_memory(builder, result_local, 0, &cst_ty)?;
            } else {
                // if we need ducktyping, we need some workspace to read the constant from db, and
                // some allocated space for the duck-typed result.
                let (result_local, _result_size) =
                    self.create_call_stack_local(builder, &expected_ty, true, true);

                let read_local = self.borrow_local(ValType::I32);
                builder
                    .global_get(self.stack_pointer)
                    .local_set(*read_local);
                let read_size = get_type_in_memory_size(&cst_ty, true);
                self.ensure_work_space(read_size as u32);

                builder
                    .local_get(*read_local)
                    .i32_const(read_size)
                    .call(self.func_by_name("stdlib.load_constant"));

                self.read_from_memory(builder, *read_local, 0, &cst_ty)?;

                self.duck_type(builder, &cst_ty, &expected_ty, Some(result_local))?;
            }

            self.charge_variable_lookup(builder, self.bindings.depth())?;
            if self.charge_local_value_copy {
                let value_ty = if need_ducktyping(&cst_ty, &expected_ty) {
                    &expected_ty
                } else {
                    &cst_ty
                };
                self.clarity_value_size_on_stack(builder, value_ty)?;
                let size = self.borrow_local(ValType::I32);
                builder.local_set(*size);
                self.charge_variable_copy(builder, *size)?;
            }
            Ok(true)
        } else {
            Ok(false)
        }
    }

    /// Save the expression on the top of the stack, with Clarity type `ty`, to
    /// local variables. If `fix_ordering` is true, then the vector is reversed
    /// so that the types are in logical order. Without this, they will be in
    /// reverse order, due to the order we pop values from the stack. Return
    /// the list of local variables.
    pub fn save_to_locals(
        &mut self,
        builder: &mut walrus::InstrSeqBuilder,
        ty: &TypeSignature,
        fix_ordering: bool,
    ) -> Vec<LocalId> {
        let wasm_types = clar2wasm_ty(ty);
        let mut locals = Vec::with_capacity(wasm_types.len());
        // Iterate in reverse order, since we are popping items off of the top
        // in reverse order.
        for wasm_ty in wasm_types.iter().rev() {
            let local = self.alloc_local(*wasm_ty);
            locals.push(local);
            builder.local_set(local);
        }

        if fix_ordering {
            // Reverse the locals to put them back in the correct order.
            locals.reverse();
        }
        locals
    }

    pub fn func_by_name(&self, name: &str) -> FunctionId {
        #[allow(clippy::unwrap_used)]
        get_function(&self.module, name).unwrap()
    }

    /// The epoch this module will execute in, when the module knows it.
    ///
    /// A module carries two epochs and they answer different questions. The
    /// semantic epoch — `contract_analysis.epoch`, the one the chain recorded at
    /// deploy time — fixes what the source *means*. The charging epoch, set by
    /// [`Self::with_cost_code_for_epoch`], is the epoch the chain is running
    /// *now*: it is where the cost table comes from, and so it is also the epoch
    /// whose runtime rules apply. A word that a later epoch withdrew is decided
    /// by this one, not by the semantic epoch.
    ///
    /// `None` only when cost code is switched off, which is a diagnostic build
    /// and never a node: `compile_for_cost_epoch` is given `emit_cost_code:
    /// true` on every production path. A caller that gets `None` must fall back
    /// to whatever runtime check the host function makes.
    pub(crate) fn executing_epoch(&self) -> Option<clarity::types::StacksEpochId> {
        self.cost_context.as_ref().map(|context| context.epoch)
    }

    pub fn get_function_type(&self, name: &str) -> Option<&FunctionType> {
        let analysis = &self.contract_analysis;

        analysis
            .get_public_function_type(name)
            .or(analysis.get_read_only_function_type(name))
            .or(analysis.get_private_function(name))
    }

    fn visit_literal_value(
        &mut self,
        builder: &mut InstrSeqBuilder,
        _expr: &SymbolicExpression,
        value: &clarity::vm::Value,
    ) -> Result<(), GeneratorError> {
        match value {
            clarity::vm::Value::Int(i) => {
                builder.i64_const((i & 0xFFFFFFFFFFFFFFFF) as i64);
                builder.i64_const(((i >> 64) & 0xFFFFFFFFFFFFFFFF) as i64);
                Ok(())
            }
            clarity::vm::Value::UInt(u) => {
                builder.i64_const((u & 0xFFFFFFFFFFFFFFFF) as i64);
                builder.i64_const(((u >> 64) & 0xFFFFFFFFFFFFFFFF) as i64);
                Ok(())
            }
            clarity::vm::Value::Sequence(SequenceData::String(s)) => {
                let (offset, len) = self.add_clarity_string_literal(s)?;
                builder.i32_const(offset as i32);
                builder.i32_const(len as i32);
                Ok(())
            }
            clarity::vm::Value::Principal(_)
            | clarity::vm::Value::Sequence(SequenceData::Buffer(_)) => {
                let (offset, len) = self.add_literal(value)?;
                builder.i32_const(offset as i32);
                builder.i32_const(len as i32);
                Ok(())
            }
            clarity::vm::Value::Bool(_)
            | clarity::vm::Value::Tuple(_)
            | clarity::vm::Value::Optional(_)
            | clarity::vm::Value::Response(_)
            | clarity::vm::Value::CallableContract(_)
            | clarity::vm::Value::Sequence(_) => Err(GeneratorError::TypeError(format!(
                "Not a valid literal type: {value:?}"
            ))),
        }
    }

    fn visit_atom(
        &mut self,
        builder: &mut InstrSeqBuilder,
        expr: &SymbolicExpression,
        atom: &ClarityName,
    ) -> Result<(), GeneratorError> {
        // Handle builtin variables
        if self.lookup_reserved_variable(builder, atom.as_str(), expr)? {
            return Ok(());
        }

        if self.lookup_constant_variable(builder, atom.as_str(), expr)? {
            return Ok(());
        }

        // Handle parameters and local bindings
        let (storage, ty, binding) = self.bindings.get_locals_and_type(atom).ok_or_else(|| {
            GeneratorError::InternalError(format!("unable to find local for {}", atom.as_str()))
        })?;

        // A spilled binding lives in the frame rather than in locals: read
        // it onto the stack and into pooled temporaries, so that every read
        // path below is unchanged. The temporaries are released before
        // returning; the binding itself has no locals to free.
        let (values, spill_temps) = match storage {
            BindingStorage::Locals(values) => (values, false),
            BindingStorage::Memory { base, delta } => {
                self.read_from_memory(builder, base, delta, &ty)?;
                (self.save_to_locals(builder, &ty, true), true)
            }
        };

        // A `let` stores a binding laid out for the type its *value* analysed
        // as, and `{ t: target, r: none }` analyses `none` as `(optional
        // NoType)`. If this occurrence is wanted as something wider — `fold`
        // sets its accumulator's type on the expression it is about to read —
        // the stored value is short by the slots the placeholder does not have,
        // and reading it as-is emits a module that will not load. Mainnet block
        // 8,667,467 is that module.
        //
        // The `let` cannot know: the type it needs comes from a use it has not
        // reached yet. So the widening happens here, where both types are in
        // hand, and only when they differ.
        let expected = self.get_expr_type(expr).cloned();
        if let Some(expected) = expected.filter(|expected| *expected != ty) {
            if let Some(actions) = widen_actions(&ty, &expected) {
                self.charge_variable_lookup(builder, self.bindings.depth())?;
                if self.charge_local_value_copy && !self.reads_owned_callable(&ty) {
                    for value in &values {
                        builder.local_get(*value);
                    }
                    self.clarity_value_size_on_stack(builder, &ty)?;
                    let size = self.borrow_local(ValType::I32);
                    builder.local_set(*size);
                    drop_value(builder, &ty);
                    self.charge_variable_copy(builder, *size)?;
                }
                let mut stored = values.iter();
                for action in actions {
                    match action {
                        Widen::Take => {
                            if let Some(value) = stored.next() {
                                builder.local_get(*value);
                            }
                        }
                        Widen::Skip => {
                            stored.next();
                        }
                        Widen::Zero(ValType::I64) => {
                            builder.i64_const(0);
                        }
                        Widen::Zero(_) => {
                            builder.i32_const(0);
                        }
                    }
                }
                if spill_temps {
                    self.release_locals(values);
                } else {
                    self.note_binding_read(binding, &values);
                }
                return Ok(());
            }
        }

        for value in &values {
            builder.local_get(*value);
        }
        self.charge_variable_lookup(builder, self.bindings.depth())?;
        if self.charge_local_value_copy && !self.reads_owned_callable(&ty) {
            self.clarity_value_size_on_stack(builder, &ty)?;
            let size = self.borrow_local(ValType::I32);
            builder.local_set(*size);
            self.charge_variable_copy(builder, *size)?;
        }
        // The gets above are this read of the binding (the copy charge
        // re-saves from the stack into its own slots); at the binding's
        // last read its slots return to the pool.
        if spill_temps {
            self.release_locals(values);
        } else {
            self.note_binding_read(binding, &values);
        }
        Ok(())
    }

    fn traverse_call_user_defined(
        &mut self,
        builder: &mut InstrSeqBuilder,
        expr: &SymbolicExpression,
        name: &ClarityName,
        args: &[SymbolicExpression],
    ) -> Result<(), GeneratorError> {
        // Epochs before 2.1 leave `none` as `NoType` instead of the optional
        // parameter type expected by the function ABI.
        let (function_args, return_ty) = match self.get_function_type(name).cloned() {
            Some(FunctionType::Fixed(FixedFunction {
                args: function_args,
                returns,
            })) => {
                check_args!(
                    self,
                    builder,
                    function_args.len(),
                    args.len(),
                    ArgumentCountCheck::Exact
                );
                for (arg, signature) in args
                    .iter()
                    .zip(function_args.iter().map(|argument| &argument.signature))
                {
                    let needs_expected_type = self.get_expr_type(arg).is_none_or(|ty| match ty {
                        TypeSignature::NoType => true,
                        TypeSignature::OptionalType(inner) => {
                            matches!(inner.as_ref(), TypeSignature::NoType)
                        }
                        _ => false,
                    });
                    if needs_expected_type {
                        self.set_expr_type(arg, signature.clone())?;
                    }
                }
                (function_args, returns)
            }
            fn_ty => {
                return Err(GeneratorError::TypeError(format!(
                    "Wrong type for a user defined function: {fn_ty:?}"
                )));
            }
        };
        let mut argument_sizes = Vec::new();
        for (arg, parameter) in args.iter().zip(&function_args) {
            let value_ty = self.value_type_before_context(arg).ok_or_else(|| {
                GeneratorError::TypeError("function argument must be typed".into())
            })?;
            self.traverse_expr(builder, arg)?;
            let size = self.borrow_local(ValType::I32);
            if let SymbolicExpressionType::LiteralValue(value) = &arg.expr {
                let value_size = i32::try_from(
                    value
                        .size()
                        .map_err(|error| GeneratorError::TypeError(error.to_string()))?,
                )
                .map_err(|_| {
                    GeneratorError::InternalError("literal argument size exceeds i32".to_owned())
                })?;
                builder.i32_const(value_size).local_set(*size);
            } else if let Some(value_size) = arg
                .match_atom()
                .and_then(|name| self.constants.get(name.as_str()))
                .filter(|ty| {
                    matches!(
                        ty,
                        TypeSignature::CallableType(CallableSubtype::Principal(_))
                    )
                })
                .and_then(|ty| ty.size().ok())
            {
                // See `constant_callable_size`: a constant holding a contract
                // principal is charged as one, not as the trait it is passed as.
                let value_size = i32::try_from(value_size).map_err(|_| {
                    GeneratorError::InternalError("constant argument size exceeds i32".to_owned())
                })?;
                builder.i32_const(value_size).local_set(*size);
            } else {
                self.clarity_value_size_on_stack(builder, &value_ty)?;
                builder.local_set(*size);
            }
            argument_sizes.push(size);
            self.duck_type(builder, &value_ty, &parameter.signature, None)?;
        }

        let expected_ty = self
            .get_expr_type(expr)
            .ok_or_else(|| {
                GeneratorError::TypeError("function call expression must be typed".to_owned())
            })?
            .clone();
        self.visit_call_user_defined(
            builder,
            name,
            &return_ty,
            Some(&expected_ty),
            None,
            Some(argument_sizes.as_slice()),
        )?;
        Ok(())
    }

    /// Visit a function call to a user-defined function. Arguments must have
    /// already been traversed and pushed to the stack.
    ///
    /// If needed, the final answer can be duck-typed to another compatible type.
    ///
    /// If needed, if some space has been pre-allocated, we can pass a local containing the offset of the space. Otherwise,
    /// the space is allocated at $stack-pointer.
    pub fn visit_call_user_defined(
        &mut self,
        builder: &mut InstrSeqBuilder,
        name: &ClarityName,
        return_ty: &TypeSignature,
        duck_ty: Option<&TypeSignature>,
        preallocated_memory: Option<LocalId>,
        argument_sizes: Option<&[BorrowedLocal]>,
    ) -> Result<(), GeneratorError> {
        // this local contains the offset at which we will copy the each new element of the result
        // if there is an in-memory type
        let in_memory_offset = has_in_memory_type(return_ty).then(|| {
            preallocated_memory.unwrap_or_else(|| {
                let return_offset = self.alloc_local(ValType::I32);

                // in case there is an in-memory type to copy, we reserve some space in memory
                let return_size = count_in_memory_space(return_ty) as i32;
                self.frame_size += return_size;

                builder
                    .global_get(self.stack_pointer)
                    .local_tee(return_offset)
                    .i32_const(return_size)
                    .binop(BinaryOp::I32Add)
                    .global_set(self.stack_pointer);

                return_offset
            })
        });

        if self
            .contract_analysis
            .get_public_function_type(name.as_str())
            .is_some()
        {
            self.local_call_public(builder, return_ty, name, argument_sizes)?;
        } else if self
            .contract_analysis
            .get_read_only_function_type(name.as_str())
            .is_some()
        {
            self.local_call_read_only(builder, name, argument_sizes)?;
        } else if self
            .contract_analysis
            .get_private_function(name.as_str())
            .is_some()
        {
            self.write_argument_sizes(
                builder,
                argument_sizes.ok_or_else(|| {
                    GeneratorError::InternalError(
                        "private call is missing its argument sizes".into(),
                    )
                })?,
            )?;
            let _ = self.local_call(builder, name, true)?;
        } else {
            return Err(GeneratorError::TypeError(format!(
                "function not found: {name}",
                name = name.as_str()
            )));
        }

        // if needed, we can convert the argument to another compatible type.
        let expected_ty = if let Some(ducky) = duck_ty {
            self.duck_type(builder, return_ty, ducky, None)?;
            ducky.clone()
        } else {
            return_ty.clone()
        };

        // If an in-memory value is returned from the function, we need to copy
        // it to our frame, from the callee's frame.
        if let Some(return_offset) = in_memory_offset {
            let locals = self.save_to_locals(builder, &expected_ty, true);
            self.copy_value(builder, &expected_ty, &locals, return_offset)?;

            for local in &locals {
                builder.local_get(*local);
            }
            self.release_locals(locals);
        }

        Ok(())
    }

    /// Call a function defined in the current contract.
    fn local_call(
        &mut self,
        builder: &mut InstrSeqBuilder,
        name: &ClarityName,
        unpack_return: bool,
    ) -> Result<Option<LocalId>, GeneratorError> {
        let function = self.user_functions.get(name).copied().ok_or_else(|| {
            GeneratorError::InternalError(format!("function {name} was not defined"))
        })?;

        let function_type = match self.get_function_type(name) {
            Some(FunctionType::Fixed(function_type)) => function_type.clone(),
            _ => {
                return Err(GeneratorError::TypeError(format!(
                    "function {name} must have a fixed type"
                )));
            }
        };
        if !uses_packed_abi(&function_type) {
            builder.call(function);
            return Ok(None);
        }

        let arguments_size = function_type
            .args
            .iter()
            .map(|argument| get_type_size(&argument.signature) as u32)
            .sum::<u32>();
        let return_size = get_type_size(&function_type.returns);
        let (arguments_offset, _) =
            self.create_call_stack_bytes(builder, arguments_size as i32 + return_size);
        let return_offset = self.alloc_local(ValType::I32);
        builder
            .local_get(arguments_offset)
            .i32_const(arguments_size as i32)
            .binop(BinaryOp::I32Add)
            .local_set(return_offset);

        let mut offsets = Vec::with_capacity(function_type.args.len());
        let mut offset = 0;
        for argument in &function_type.args {
            offsets.push(offset);
            offset += u32::try_from(get_type_size(&argument.signature)).map_err(|_| {
                GeneratorError::InternalError(
                    "negative packed argument representation size".to_owned(),
                )
            })?;
        }
        // Arguments are on the operand stack in declaration order. Consume
        // them from the top down directly into their fixed memory offsets;
        // flattening them into locals recreates the validator limit the packed
        // ABI exists to avoid.
        for (argument, offset) in function_type.args.iter().zip(offsets).rev() {
            self.write_to_memory(builder, arguments_offset, offset, &argument.signature)?;
        }

        builder
            .local_get(arguments_offset)
            .local_get(return_offset)
            .call(function);
        if unpack_return {
            self.read_from_memory(builder, return_offset, 0, &function_type.returns)?;
            return Ok(None);
        }

        Ok(Some(return_offset))
    }

    /// Call a public function defined in the current contract. This requires
    /// going through the host interface to handle roll backs.
    fn local_call_public(
        &mut self,
        builder: &mut InstrSeqBuilder,
        return_ty: &TypeSignature,
        name: &ClarityName,
        argument_sizes: Option<&[BorrowedLocal]>,
    ) -> Result<(), GeneratorError> {
        self.write_argument_sizes(
            builder,
            argument_sizes.ok_or_else(|| {
                GeneratorError::InternalError("public call is missing argument sizes".into())
            })?,
        )?;

        // Call the host interface function, `begin_public_call`
        builder.call(self.func_by_name("stdlib.begin_public_call"));

        let packed_return = self.local_call(builder, name, false)?;
        let result_locals = packed_return
            .is_none()
            .then(|| self.save_to_locals(builder, return_ty, true));

        // If the result is an `ok`, then we can commit the call, and if it
        // is an `err`, then we roll it back. `result_locals[0]` is the
        // response indicator (all public functions return a response).
        let if_id = {
            let mut if_case: InstrSeqBuilder<'_> = builder.dangling_instr_seq(None);
            if_case.call(self.func_by_name("stdlib.commit_call"));
            if_case.id()
        };

        let else_id = {
            let mut else_case: InstrSeqBuilder<'_> = builder.dangling_instr_seq(None);
            else_case.call(self.func_by_name("stdlib.roll_back_call"));
            else_case.id()
        };

        if let Some(return_offset) = packed_return {
            builder.local_get(return_offset).load(
                self.get_memory()?,
                LoadKind::I32 { atomic: false },
                MemArg {
                    align: 4,
                    offset: 0,
                },
            );
        } else if let Some(result_locals) = &result_locals {
            builder.local_get(result_locals[0]);
        }
        builder.instr(IfElse {
            consequent: if_id,
            alternative: else_id,
        });

        // Restore the result to the top of the stack.
        if let Some(return_offset) = packed_return {
            self.read_from_memory(builder, return_offset, 0, return_ty)?;
        } else if let Some(result_locals) = result_locals {
            for local in &result_locals {
                builder.local_get(*local);
            }
            self.release_locals(result_locals);
        }

        Ok(())
    }

    /// Return the type describing the Wasm layout an expression's traversal
    /// leaves on the stack, which is not always the analyzer's contextual type
    /// and not always the value's own. A constant, binding, or user-function
    /// result is produced with its own source type — that sameness reproduces
    /// the interpreter's entry charge, since `runtime_size` reads sizes from
    /// the value either way — but when the read converts it to the contextual
    /// type (a binding widened out of a `NoType` placeholder layout, a constant
    /// or call result duck-typed), the stack holds the contextual layout and
    /// measuring the source layout against it emits a module that will not
    /// load. Mainnet contract `fastpool-max500-signer-manager` passes a
    /// let-bound `{bond-index: none}` to a `(optional uint)` parameter and is
    /// that module.
    pub(crate) fn value_type_before_context(
        &self,
        expr: &SymbolicExpression,
    ) -> Option<TypeSignature> {
        type ConversionRule = fn(&TypeSignature, &TypeSignature) -> bool;
        fn widens(stored: &TypeSignature, expected: &TypeSignature) -> bool {
            widen_actions(stored, expected).is_some()
        }
        let contextual = self.get_expr_type(expr);
        let (produced, converts): (_, ConversionRule) = match &expr.expr {
            SymbolicExpressionType::Atom(name) => {
                if let Some(constant) = self.constants.get(name.as_str()) {
                    (Some(constant.clone()), need_ducktyping)
                } else {
                    let binding = self.bindings.get_locals_and_type(name).map(|(_, ty, _)| ty);
                    (binding, widens)
                }
            }
            SymbolicExpressionType::List(items) => {
                let returns = items
                    .first()
                    .and_then(SymbolicExpression::match_atom)
                    .and_then(|name| self.get_function_type(name.as_str()))
                    .and_then(|function| match function {
                        FunctionType::Fixed(function) => Some(function.returns.clone()),
                        _ => None,
                    });
                (returns, need_ducktyping)
            }
            _ => (None, need_ducktyping),
        };
        let Some(produced) = produced else {
            return contextual.cloned();
        };
        match contextual {
            Some(expected) if *expected != produced && converts(&produced, expected) => {
                Some(expected.clone())
            }
            _ => Some(produced),
        }
    }

    pub(crate) fn is_user_defined_function(&self, name: &str) -> bool {
        self.get_function_type(name).is_some()
    }

    /// Measure the value on top of the stack and keep the size for the callee.
    ///
    /// The value is left where it was found: this is a measurement, not a
    /// consumption.
    pub(crate) fn take_argument_size(
        &mut self,
        builder: &mut InstrSeqBuilder,
        ty: &TypeSignature,
    ) -> Result<BorrowedLocal, GeneratorError> {
        let size = self.borrow_local(ValType::I32);
        self.clarity_value_size_on_stack(builder, ty)?;
        builder.local_set(*size);
        Ok(size)
    }

    fn admit_runtime_shape_parameter(
        &mut self,
        builder: &mut InstrSeqBuilder,
        ty: &TypeSignature,
        storage: &BindingStorage,
        function_name_offset: u32,
        function_name_length: u32,
        argument_index: usize,
    ) -> Result<(), GeneratorError> {
        match storage {
            BindingStorage::Locals(locals) => {
                for local in locals {
                    builder.local_get(*local);
                }
            }
            BindingStorage::Memory { base, delta } => {
                self.read_from_memory(builder, *base, *delta, ty)?;
            }
        }

        // The host first reads the original value, so it can safely overwrite
        // this region with the admitted representation and its pointed data.
        // The region remains in the callee frame for the whole function body.
        let (value_offset, _) = self.create_call_stack_local(builder, ty, true, true);
        self.write_to_memory(builder, value_offset, 0, ty)?;
        let (type_offset, type_length) = self.serialized_type(ty)?;
        let argument_index = i32::try_from(argument_index).map_err(|_| {
            GeneratorError::InternalError("function argument index exceeds i32".into())
        })?;
        builder
            .local_get(value_offset)
            .i32_const(type_offset)
            .i32_const(type_length)
            .i32_const(function_name_offset as i32)
            .i32_const(function_name_length as i32)
            .i32_const(argument_index)
            .call(self.func_by_name("stdlib.admit_function_argument"));
        self.read_from_memory(builder, value_offset, 0, ty)?;

        match storage {
            BindingStorage::Locals(locals) => {
                for local in locals.iter().rev() {
                    builder.local_set(*local);
                }
            }
            BindingStorage::Memory { base, delta } => {
                self.write_to_memory(builder, *base, *delta, ty)?;
            }
        }

        // The admitted value is the sanitised one, and the reference sanitises
        // back to what the data says: a field that arrived carrying its
        // parent's declared width is read inside the callee at its own. The
        // representation now holds that value whole, so the arena entry it came
        // with is both redundant and wrong to keep — measuring through it would
        // answer the caller's width for the callee's binding.
        if carries_runtime_shape(ty) {
            match storage {
                BindingStorage::Locals(locals) => {
                    if let Some(handle) = locals.first() {
                        builder.i32_const(0).local_set(*handle);
                    }
                }
                BindingStorage::Memory { base, delta } => {
                    let memory = self.get_memory()?;
                    builder.local_get(*base).i32_const(0).store(
                        memory,
                        walrus::ir::StoreKind::I32 { atomic: false },
                        walrus::ir::MemArg {
                            align: 4,
                            offset: *delta,
                        },
                    );
                }
            }
        }
        Ok(())
    }

    /// Call a read-only function defined in the current contract.
    fn local_call_read_only(
        &mut self,
        builder: &mut InstrSeqBuilder,
        name: &ClarityName,
        argument_sizes: Option<&[BorrowedLocal]>,
    ) -> Result<(), GeneratorError> {
        self.write_argument_sizes(
            builder,
            argument_sizes.ok_or_else(|| {
                GeneratorError::InternalError("read-only call is missing argument sizes".into())
            })?,
        )?;

        // Call the host interface function, `begin_readonly_call`
        builder.call(self.func_by_name("stdlib.begin_read_only_call"));

        let _ = self.local_call(builder, name, true)?;

        // Call the host interface function, `roll_back_call`
        builder.call(self.func_by_name("stdlib.roll_back_call"));

        Ok(())
    }

    fn write_argument_sizes(
        &self,
        builder: &mut InstrSeqBuilder,
        argument_sizes: &[BorrowedLocal],
    ) -> Result<(), GeneratorError> {
        let memory = self.get_memory()?;
        for (index, size) in argument_sizes.iter().enumerate() {
            let offset = u32::try_from(index)
                .ok()
                .and_then(|index| index.checked_mul(4))
                .ok_or_else(|| {
                    GeneratorError::InternalError("function argument-size offset overflow".into())
                })?;
            builder
                .global_get(self.argument_sizes)
                .local_get(**size)
                .store(
                    memory,
                    StoreKind::I32 { atomic: false },
                    MemArg { align: 4, offset },
                );
        }
        Ok(())
    }

    pub fn traverse_args(
        &mut self,
        builder: &mut InstrSeqBuilder,
        args: &[SymbolicExpression],
    ) -> Result<(), GeneratorError> {
        for arg in args.iter() {
            self.traverse_expr(builder, arg)?;
        }
        Ok(())
    }

    pub fn debug_msg<M: Into<String>>(&mut self, builder: &mut InstrSeqBuilder, message: M) {
        let id = debug_msg::register(message.into());
        builder.i32_const(id);
        builder.call(self.func_by_name("debug_msg"));
    }

    /// Dump the top of the stack to debug messages
    pub fn debug_dump_stack<M: Into<String>>(
        &mut self,
        builder: &mut InstrSeqBuilder,
        message: M,
        expected_types: &[ValType],
    ) {
        self.debug_msg(builder, message);
        self.debug_msg(builder, "<stack dump start>");
        let mut locals = vec![];

        for t in expected_types {
            let l = self.borrow_local(*t);
            builder.local_tee(*l);
            locals.push(l);
            match t {
                ValType::I32 => self.debug_log_i32(builder),
                ValType::I64 => self.debug_log_i64(builder),
                _ => {
                    // allow unimplemented in debug code
                    #[allow(clippy::unimplemented)]
                    {
                        unimplemented!("unsupported stack dump type")
                    }
                }
            }
        }
        self.debug_msg(builder, "<stack dump end>");

        // restore the stack
        while let Some(l) = locals.pop() {
            builder.local_get(*l);
        }
    }

    pub fn debug_log_local_i32<M: Into<String>>(
        &mut self,
        builder: &mut InstrSeqBuilder,
        message: M,
        local_id: &LocalId,
    ) {
        self.debug_msg(builder, message);
        builder.local_get(*local_id);
        self.debug_log_i32(builder)
    }

    pub fn debug_log_local_i64<M: Into<String>>(
        &mut self,
        builder: &mut InstrSeqBuilder,
        message: M,
        local_id: &LocalId,
    ) {
        self.debug_msg(builder, message);
        builder.local_get(*local_id);
        self.debug_log_i64(builder)
    }

    #[allow(dead_code)]
    /// Log an i64 that is on top of the stack.
    pub fn debug_log_i64(&self, builder: &mut InstrSeqBuilder) {
        builder.call(self.func_by_name("log"));
    }

    #[allow(dead_code)]
    /// Log an i32 that is on top of the stack.
    pub fn debug_log_i32(&self, builder: &mut InstrSeqBuilder) {
        builder
            .unop(UnaryOp::I64ExtendUI32)
            .call(self.func_by_name("log"));
    }

    pub(crate) fn is_reserved_name(&self, name: &ClarityName) -> bool {
        let version = self.contract_analysis.clarity_version;

        functions::lookup_reserved_functions(name.as_str(), &version).is_some()
            || variables::is_reserved_name(name, &version)
    }

    pub fn get_sequence_element_type(
        &self,
        sequence: &SymbolicExpression,
    ) -> Result<SequenceElementType, GeneratorError> {
        match self.get_expr_type(sequence).ok_or_else(|| {
            GeneratorError::TypeError("sequence expression must be typed".to_owned())
        })? {
            TypeSignature::SequenceType(seq_ty) => match &seq_ty {
                SequenceSubtype::ListType(list_type) => Ok(SequenceElementType::Other(
                    list_type.get_list_item_type().clone(),
                )),
                SequenceSubtype::BufferType(_)
                | SequenceSubtype::StringType(StringSubtype::ASCII(_)) => {
                    // For buffer and string-ascii return none, which indicates
                    // that elements should be read byte-by-byte.
                    Ok(SequenceElementType::Byte)
                }
                SequenceSubtype::StringType(StringSubtype::UTF8(_)) => {
                    Ok(SequenceElementType::UnicodeScalar)
                }
            },
            _ => Err(GeneratorError::TypeError(
                "expected sequence type".to_string(),
            )),
        }
    }

    /// Ensure enough work space is going to be available in memory
    pub(crate) fn ensure_work_space(&mut self, bytes_len: u32) {
        self.max_work_space = self.max_work_space.max(bytes_len);
    }

    pub(crate) fn get_current_function_return_type(&self) -> Option<&TypeSignature> {
        self.current_function_type.as_ref().map(|f| &f.returns)
    }

    pub(crate) fn current_function_wasm_return_types(&self) -> Option<Vec<ValType>> {
        self.current_function_type.as_ref().map(|function| {
            if self.packed_return_offset.is_some() {
                Vec::new()
            } else {
                clar2wasm_ty(&function.returns)
            }
        })
    }

    pub(crate) fn get_current_function_arg_type(
        &self,
        arg_name: &ClarityName,
    ) -> Option<&TypeSignature> {
        self.current_function_type
            .as_ref()
            .map(|f| &f.args)
            .and_then(|args| {
                args.iter()
                    .find_map(|arg| (&arg.name == arg_name).then_some(&arg.signature))
            })
    }
}

/// Returns true if a composed type has an inner in-memory type.
pub fn has_in_memory_type(ty: &TypeSignature) -> bool {
    match ty {
        TypeSignature::OptionalType(opt) => has_in_memory_type(opt),
        TypeSignature::ResponseType(resp) => {
            has_in_memory_type(&resp.0) || has_in_memory_type(&resp.1)
        }
        TypeSignature::TupleType(tup) => tup.get_type_map().values().any(has_in_memory_type),
        TypeSignature::NoType
        | TypeSignature::IntType
        | TypeSignature::UIntType
        | TypeSignature::BoolType => false,
        TypeSignature::SequenceType(_)
        | TypeSignature::PrincipalType
        | TypeSignature::CallableType(_)
        | TypeSignature::ListUnionType(_)
        | TypeSignature::TraitReferenceType(_) => true,
    }
}

/// Counts the amount of bytes needed in memory for a type.
fn count_in_memory_space(ty: &TypeSignature) -> u32 {
    match ty {
        TypeSignature::BoolType
        | TypeSignature::IntType
        | TypeSignature::UIntType
        | TypeSignature::NoType => 0,
        TypeSignature::OptionalType(opt) => count_in_memory_space(opt),
        TypeSignature::ResponseType(resp) => {
            count_in_memory_space(&resp.0) + count_in_memory_space(&resp.1)
        }
        TypeSignature::PrincipalType
        | TypeSignature::CallableType(_)
        | TypeSignature::ListUnionType(_)
        | TypeSignature::TraitReferenceType(_) => PRINCIPAL_BYTES_MAX as u32,
        TypeSignature::SequenceType(SequenceSubtype::BufferType(len))
        | TypeSignature::SequenceType(SequenceSubtype::StringType(StringSubtype::ASCII(len))) => {
            len.into()
        }
        TypeSignature::SequenceType(SequenceSubtype::StringType(StringSubtype::UTF8(len))) => {
            4 * u32::from(len)
        }
        TypeSignature::SequenceType(SequenceSubtype::ListType(ltd)) => {
            ltd.get_max_len() * get_type_in_memory_size(ltd.get_list_item_type(), true) as u32
        }
        TypeSignature::TupleType(tup) => {
            tup.get_type_map().values().map(count_in_memory_space).sum()
        }
    }
}

/// Whether a function body can switch the sender, and so needs the prologue that
/// records the principal stacks' depth.
///
/// Conservative on purpose, and the asymmetry is the whole design: a wrong `true`
/// costs two locals and two calls, while a wrong `false` lets a switched sender
/// escape a function and be inherited by whatever runs next -- which is mainnet
/// block 8,668,161, where a function that `asserts!` its way out of `as-contract`
/// was called twice by `map` and the second call transferred to itself.
///
/// So this asks only whether the *name* appears anywhere in the body's tree,
/// including inside `let` bodies, branches and arguments. It does not try to decide
/// whether the call is reachable, and it does not follow calls into other functions:
/// `as-contract` switches the sender for the dynamic extent of its own body, which
/// ends before any callee's postlude runs, so a callee cannot leak into its caller.
fn body_contains_as_contract(body: &SymbolicExpression) -> bool {
    match &body.expr {
        SymbolicExpressionType::Atom(name) => name.as_str() == "as-contract",
        SymbolicExpressionType::List(expressions) => {
            expressions.iter().any(body_contains_as_contract)
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use std::env;

    use clarity::types::StacksEpochId;
    use clarity::vm::analysis::AnalysisDatabase;
    use clarity::vm::costs::LimitedCostTracker;
    use clarity::vm::database::MemoryBackingStore;
    use clarity::vm::errors::{RuntimeCheckErrorKind, VmExecutionError};
    use clarity::vm::types::{QualifiedContractIdentifier, StandardPrincipalData, TupleData};
    use clarity::vm::{ClarityVersion, Value};
    use clarity_types::{ClarityName, ContractName};
    use walrus::Module;

    // Tests that don't relate to specific words
    use crate::{
        compile,
        tools::{crosscheck, crosscheck_compare_only, crosscheck_cost, evaluate},
        wasm_generator::{EmittedLocals, LocalsReport, END_OF_STANDARD_DATA},
    };

    #[test]
    fn emitted_locals_are_measured_from_the_final_wasm() {
        let wasm = wat::parse_str(
            "(module
                (import \"host\" \"f\" (func (param i32)))
                (func $named (export \"named-export\")
                    (param i32 i64)
                    (local i32 i32 i64)))",
        )
        .expect("valid Wasm fixture");
        let mut report = LocalsReport::default();
        report
            .measure_emitted(&wasm)
            .expect("the final Wasm can be measured");
        assert_eq!(
            report.emitted.get("named"),
            Some(&EmittedLocals {
                parameters: 2,
                declared: 3,
                total: 5,
            })
        );
    }

    fn one_exact_price_feed() -> Value {
        Value::some(
            Value::cons_list_unsanitized(vec![
                Value::buff_from(vec![0x5a; 2_007]).expect("a valid price feed")
            ])
            .expect("a valid price-feed list"),
        )
        .expect("a valid optional price-feed list")
    }

    #[test]
    fn runtime_shape_cost_uses_actual_list_entry_size() {
        crosscheck_cost(
            "(define-public (consume (feeds (optional (list 3 (buff 8192)))))
               (ok feeds))",
            "consume",
            &[one_exact_price_feed()],
        );
    }

    #[test]
    fn runtime_value_cost_survives_a_binding_and_local_call() {
        crosscheck_cost(
            "(define-public (collateral-add (feeds (optional (list 3 (buff 8192)))))
               (ok feeds))
             (define-public (supply-collateral-add
                 (price-feed (buff 8192)))
               (let ((payload { feeds: (some (list price-feed)) }))
                 (collateral-add (get feeds payload))))",
            "supply-collateral-add",
            &[Value::buff_from(vec![0x5a; 2_007]).expect("a valid price feed")],
        );
    }

    #[test]
    fn runtime_value_cost_slices_response_arms() {
        let entry = Value::Tuple(
            TupleData::from_data(vec![
                (ClarityName::from_literal("gas-price"), Value::UInt(12)),
                (ClarityName::from_literal("price"), Value::UInt(34)),
            ])
            .expect("valid chain data"),
        );
        crosscheck_cost(
            "(define-public (consume
                 (entry (response { gas-price: uint, price: uint } uint)))
               (ok true))",
            "consume",
            &[Value::okay(entry).expect("a valid response")],
        );
    }

    #[test]
    fn runtime_value_cost_skips_inactive_composite_arms() {
        crosscheck_cost(
            "(define-public (consume
                 (entry (response { addr: principal } uint)))
               (ok true))",
            "consume",
            &[Value::error(Value::UInt(7)).expect("a valid response")],
        );
        crosscheck_cost(
            "(define-public (consume
                 (entry (optional { addr: principal })))
               (ok true))",
            "consume",
            &[Value::none()],
        );
    }

    #[test]
    fn runtime_value_cost_loads_the_gas_oracle_shape() {
        crosscheck_cost(
            "(define-constant err-not-found (err u4001))
             (define-map chain-data uint { price: uint, gas-price: uint })
             (define-read-only (get-chain-data (id uint))
               (match (map-get? chain-data id)
                 entry (ok entry)
                 err-not-found))",
            "get-chain-data",
            &[Value::UInt(0)],
        );
    }

    #[test]
    fn is_in_regtest() {
        crosscheck(
            "
(define-public (regtest)
  (ok is-in-regtest))

(regtest)
",
            evaluate("(ok false)"),
        );
    }

    #[test]
    fn should_set_memory_pages() {
        let string_size = 262000;
        let a = "a".repeat(string_size);
        let b = "b".repeat(string_size);
        let c = "c".repeat(string_size);
        let d = "d".repeat(string_size);

        let snippet = format!("(is-eq u\"{a}\" u\"{b}\" u\"{c}\" u\"{d}\")");
        crosscheck(&snippet, Ok(Some(clarity::vm::Value::Bool(false))));
    }

    #[test]
    fn test_work_space() {
        let buff_len = 1048576;
        let buff = "aa".repeat(buff_len);

        let get_initial_memory = |snippet: String| {
            let module = compile(
                &snippet,
                &QualifiedContractIdentifier::new(
                    StandardPrincipalData::transient(),
                    ContractName::from_literal("tmp"),
                ),
                LimitedCostTracker::new_free(),
                ClarityVersion::Clarity2,
                StacksEpochId::Epoch25,
                &mut AnalysisDatabase::new(&mut MemoryBackingStore::new()),
                false,
            )
            .unwrap()
            .module;
            let mem = module.memories.iter().next().unwrap().initial;
            mem
        };
        let prologue = format!("(let ((foo 0x{buff})) ");
        // sha256 requires some extra work space, thus extra pages
        assert!(
            get_initial_memory(format!("{prologue} (len foo))"))
                < get_initial_memory(format!("{prologue} (sha256 foo))"))
        );
        // but multiple calls do not cause more pages
        assert_eq!(
            get_initial_memory(format!("{prologue} (sha256 foo))")),
            get_initial_memory(format!("{prologue} (sha256 foo) (sha256 foo))"))
        );
    }

    /// The `poc2` witness from `benches/comparison.rs`: a wide nested tuple
    /// bound once, then read `copies` times through a list. Every read pays
    /// a copy charge that saves the value to locals, which used to allocate
    /// ~518 fresh locals per read, so 100 copies overflowed wasmtime's
    /// 50,000-locals-per-function limit on a source the interpreter accepts.
    fn poc2_source(copies: usize) -> String {
        format!(
            r#"
            (define-public (poc2 (v int))
                (begin
                    (let ((a {{a: {{a: {{b: 1,c: 1,d: 1,e: 1,f: 1,g: 1,h: 1,i: 1,j: 1,k: 1,l: 1,m: 1,n: 1,o: 1,p: 1,q: 1,r: 1,s: 1,t: 1,u-: 1,v: 1,w: 1,x: 1,y: 1,z: 1,A: 1,B: 1,C: 1,D: 1,E: 1,F: 1,G: 1,H: 1,I: 1,J: 1,K: 1,L: 1,M: 1,N: 1,O: 1,P: 1,Q: 1,R: 1,S: 1,T: 1,U: 1,V: 1,W: 1,X: 1,Y: 1,Z: 1,ba: 1,bb: 1,bc: 1,bd: 1,be: 1,bf: 1,bg: 1,bh: 1,bi: 1,bj: 1,bk: 1,bl: 1,bm: 1,bn: 1,bo: 1,bp: 1,bq: 1,br: 1,bs: 1,bt: 1,bu: 1,bv: 1,bw: 1,bx: 1,by: 1,bz: 1,bA: 1,bB: 1,bC: 1,bD: 1,bE: 1,bF: 1,bG: 1,bH: 1,bI: 1,bJ: 1,bK: 1,bL: 1,bM: 1,bN: 1,bO: 1,bP: 1,bQ: 1,bR: 1,bS: 1,bT: 1,bU: 1,bV: 1,bW: 1,bX: 1,bY: 1,bZ: 1,ca: 1,cb: 1,cc: 1,cd: 1,ce: 1,cf: 1,cg: 1,ch: 1,ci: 1,cj: 1,ck: 1,cl: 1,cm: 1,cn: 1,co: 1,cp: 1,cq: 1,cr: 1,cs: 1,ct: 1,cu: 1,cv: 1,cw: 1,cx: 1,cy: 1,cz: 1,cA: 1,cB: 1,cC: 1,cD: 1,cE: 1,cF: 1,cG: 1,cH: 1,cI: 1,cJ: 1,cK: 1,cL: 1,cM: 1,cN: 1,cO: 1,cP: 1,cQ: 1,cR: 1,cS: 1,cT: 1,cU: 1,cV: 1,cW: 1,cX: 1,cY: 1,cZ: 1,da: 1,db: 1,dc: 1,dd: 1,de: 1,df: 1,dg: 1,dh: 1,di: 1,dj: 1,dk: 1,dl: 1,dm: 1,dn: 1,do: 1,dp: 1,dq: 1,dr: 1,ds: 1,dt: 1,du: 1,dv: 1,dw: 1,dx: 1,dy: 1,dz: 1,dA: 1,dB: 1,dC: 1,dD: 1,dE: 1,dF: 1,dG: 1,dH: 1,dI: 1,dJ: 1,dK: 1,dL: 1,dM: 1,dN: 1,dO: 1,dP: 1,dQ: 1,dR: 1,dS: 1,dT: 1,dU: 1,dV: 1,dW: 1,dX: 1,dY: 1,dZ: 1,ea: 1,eb: 1,ec: 1,ed: 1,ee: 1,ef: 1,eg: 1,eh: 1,ei: 1,ej: 1,ek: 1,el: 1,em: 1,en: 1,eo: 1,ep: 1,eq: 1,er: 1,es: 1,et: 1,eu: 1,ev: 1,ew: 1,ex: 1,ey: 1,ez: 1,eA: 1,eB: 1,eC: 1,eD: 1,eE: 1,eF: 1,eG: 1,eH: 1,eI: 1,eJ: 1,eK: 1,eL: 1,eM: 1,eN: 1,eO: 1}}}}}}) (b (list{} ))) b)
                    (ok (+ 1 1))
                )
            )
            (poc2 42)"#,
            " a".repeat(copies)
        )
    }

    #[test]
    fn wide_tuple_read_many_times_stays_loadable() {
        let snippet = poc2_source(100);

        // The interpreter and the compiler agree on the result.
        crosscheck(
            &snippet,
            Ok(Some(Value::okay(Value::Int(2)).expect("ok response"))),
        );

        // And the emitted module loads.
        let mut compiled = compile(
            &snippet,
            &QualifiedContractIdentifier::new(
                StandardPrincipalData::transient(),
                ContractName::from_literal("poc2"),
            ),
            LimitedCostTracker::new_free(),
            ClarityVersion::latest(),
            StacksEpochId::latest(),
            &mut AnalysisDatabase::new(&mut MemoryBackingStore::new()),
            true,
        )
        .expect("poc2 compiles");
        wasmtime::Module::new(
            &crate::consensus_engine().expect("the consensus engine"),
            compiled.module.emit_wasm(),
        )
        .expect("wasmtime loads the module");

        // The measured peak is far under wasmtime's 50,000-locals limit: the
        // binding lives once and every read reuses the same slots. Before
        // scoped local reuse the same source peaked at ~51,800.
        let peak = compiled
            .locals_report
            .max_live_locals
            .values()
            .max()
            .copied()
            .unwrap_or(0);
        assert!(
            peak < 10_000,
            "poc2 at 100 copies peaks at {peak} live locals"
        );
    }

    #[test]
    fn more_simultaneous_bindings_than_wasmtime_allows_still_compiles() {
        // 60,000 bindings of which only `a0` is read: the unused bindings are
        // dropped instead of saved, so the emitted module is small and
        // wasmtime loads it. nano-conformance's `engine_failure.rs` used to
        // force a module-load refusal with this shape; it now uses a return
        // value wider than wasmtime's function-type limit (see
        // `more_wasm_returns_than_wasmtime_allows_uses_packed_abi_and_loads`).
        let bindings = (0..60_000_u32)
            .map(|index| format!("(a{index} u1)"))
            .collect::<Vec<_>>()
            .join(" ");
        let snippet = format!("(define-public (f) (ok (let ({bindings}) a0)))");
        let mut compiled = compile(
            &snippet,
            &QualifiedContractIdentifier::new(
                StandardPrincipalData::transient(),
                ContractName::from_literal("wide-let"),
            ),
            LimitedCostTracker::new_free(),
            ClarityVersion::latest(),
            StacksEpochId::latest(),
            &mut AnalysisDatabase::new(&mut MemoryBackingStore::new()),
            true,
        )
        .expect("a let wider than wasmtime's locals limit still compiles");

        // Only `a0` is live at the body, so the measured peak is far under
        // wasmtime's limit...
        let peak = compiled.locals_report.max_live_locals["f"];
        assert!(
            peak < 50_000,
            "60,000 bindings of which one is read should measure well under the limit, got {peak}"
        );

        // ...and the runtime accepts the module.
        wasmtime::Module::new(
            &crate::consensus_engine().expect("the consensus engine"),
            compiled.module.emit_wasm(),
        )
        .expect("wasmtime loads the module");
    }

    #[test]
    fn all_bindings_read_at_once_stays_loadable() {
        // Every binding is read by the final `list`, so all 26,000 are live
        // at its construction point and no liveness pass can free them. A
        // scope this wide spills its bindings to the frame: the function
        // declares no locals for them, and wasmtime loads the module.
        let bindings = (0..26_000_u32)
            .map(|index| format!("(a{index} u1)"))
            .collect::<Vec<_>>()
            .join(" ");
        let uses = (0..26_000_u32)
            .map(|index| format!("a{index}"))
            .collect::<Vec<_>>()
            .join(" ");
        let snippet = format!("(define-public (f) (ok (let ({bindings}) (list {uses}))))");

        // The interpreter and the compiler agree on the result.
        crosscheck_compare_only(&snippet);

        let mut compiled = compile(
            &snippet,
            &QualifiedContractIdentifier::new(
                StandardPrincipalData::transient(),
                ContractName::from_literal("wide-let-all-used"),
            ),
            LimitedCostTracker::new_free(),
            ClarityVersion::latest(),
            StacksEpochId::latest(),
            &mut AnalysisDatabase::new(&mut MemoryBackingStore::new()),
            true,
        )
        .expect("a let whose bindings are all read still compiles");

        // The bindings live in the frame, so the measured peak is far under
        // wasmtime's limit (52,006 live locals before spilling)...
        let peak = compiled.locals_report.max_live_locals["f"];
        assert!(
            peak < 50_000,
            "26,000 spilled bindings should measure well under the limit, got {peak}"
        );

        // ...and the runtime accepts the module.
        wasmtime::Module::new(
            &crate::consensus_engine().expect("the consensus engine"),
            compiled.module.emit_wasm(),
        )
        .expect("wasmtime loads the module");
    }

    fn uint_tuple(fields: usize, values: bool) -> String {
        (0..fields)
            .map(|index| {
                if values {
                    format!("f{index}: u{index}")
                } else {
                    format!("f{index}: uint")
                }
            })
            .collect::<Vec<_>>()
            .join(", ")
    }

    fn loadable_report(source: &str, name: &str) -> LocalsReport {
        let mut compiled = compile(
            source,
            &QualifiedContractIdentifier::new(
                StandardPrincipalData::transient(),
                ContractName::try_from(name.to_owned()).expect("generated contract name is valid"),
            ),
            LimitedCostTracker::new_free(),
            ClarityVersion::latest(),
            StacksEpochId::latest(),
            &mut AnalysisDatabase::new(&mut MemoryBackingStore::new()),
            true,
        )
        .unwrap_or_else(|error| panic!("{name} compiles: {error:?}"));
        let mut report = compiled.locals_report.clone();
        let wasm = compiled.module.emit_wasm();
        report
            .measure_emitted(&wasm)
            .unwrap_or_else(|error| panic!("{name} emitted locals are measurable: {error}"));
        wasmtime::Module::new(
            &crate::consensus_engine().expect("the consensus engine"),
            wasm,
        )
        .unwrap_or_else(|error| panic!("{name} loads: {error}"));
        report
    }

    fn loadable_peak(source: &str, name: &str) -> u32 {
        loadable_report(source, name)
            .max_live_locals
            .values()
            .copied()
            .max()
            .unwrap_or(0)
    }

    fn assert_emitted_locals_below_limit(source: &str, name: &str) {
        let report = loadable_report(source, name);
        let (function, measurement) = report
            .emitted
            .iter()
            .max_by_key(|(_, measurement)| measurement.total)
            .expect("a module defines functions");
        assert!(
            measurement.total < 50_000,
            "{name} emitted {function} with {} parameters+locals",
            measurement.total
        );
    }

    fn emitted_function_locals(source: &str, contract: &str, function: &str) -> u32 {
        let report = loadable_report(source, contract);
        report
            .emitted
            .get(function)
            .unwrap_or_else(|| {
                panic!(
                    "{contract} emitted no {function:?} function; names were {:?}",
                    report.emitted.keys().collect::<Vec<_>>()
                )
            })
            .total
    }

    #[test]
    fn cumulative_nested_bindings_spill_by_flattened_slots() {
        let tuple = uint_tuple(300, true);
        let source = format!(
            "(define-read-only (f)
               (let ((outer {{{tuple}}}))
                 (let ((inner outer))
                   (is-eq (get f0 outer) (get f0 inner)))))
             (f)"
        );
        crosscheck_compare_only(&source);
        assert!(loadable_peak(&source, "nested-spill") < 50_000);
    }

    #[test]
    fn wide_optional_match_binding_stays_loadable() {
        let tuple = uint_tuple(600, true);
        let source = format!(
            "(define-read-only (f (present bool))
               (match (if present (some {{{tuple}}}) none)
                 value (get f0 value)
                 u0))
             (list (f true) (f false))"
        );
        crosscheck_compare_only(&source);
        assert!(loadable_peak(&source, "wide-optional-match") < 50_000);
    }

    #[test]
    fn response_match_plans_both_payloads_together() {
        let ok = uint_tuple(300, true);
        let err = uint_tuple(300, true);
        let source = format!(
            "(define-read-only (f (succeed bool))
               (match (if succeed (ok {{{ok}}}) (err {{{err}}}))
                 ok-value (get f0 ok-value)
                 err-value (get f0 err-value)))
             (list (f true) (f false))"
        );
        crosscheck_compare_only(&source);
        assert!(loadable_peak(&source, "wide-response-match") < 50_000);
    }

    #[test]
    fn packed_parameters_never_expand_into_function_locals() {
        let tuple_type = uint_tuple(600, false);
        let tuple_value = uint_tuple(600, true);
        let source = format!(
            "(define-private (identity (value {{{tuple_type}}})) value)
             (define-public (f) (ok (get f0 (identity {{{tuple_value}}}))))
             (f)"
        );
        crosscheck_compare_only(&source);
        assert!(loadable_peak(&source, "packed-parameter") < 50_000);
    }

    #[test]
    fn sequential_wide_try_values_reuse_their_emitted_locals() {
        let tuple = uint_tuple(600, true);
        let calls = std::iter::repeat_n("(try! (wide))", 45)
            .collect::<Vec<_>>()
            .join(" ");
        let source = format!(
            "(define-private (wide) (if true (ok {{{tuple}}}) (err u0)))
             (define-public (f) (begin {calls} (ok true)))
             (f)"
        );
        crosscheck_compare_only(&source);
        assert_emitted_locals_below_limit(&source, "wide-sequential-try");
    }

    #[test]
    fn packed_public_responses_are_inspected_without_flattening() {
        let tuple = uint_tuple(600, true);
        let calls = std::iter::repeat_n("(try! (wide))", 45)
            .collect::<Vec<_>>()
            .join(" ");
        let source = format!(
            "(define-public (wide) (if true (ok {{{tuple}}}) (err u0)))
             (define-public (f) (begin {calls} (ok true)))
             (f)"
        );
        crosscheck_compare_only(&source);
        assert_emitted_locals_below_limit(&source, "packed-public-response");
        assert!(
            emitted_function_locals(&source, "packed-public-response", "f") < 5_000,
            "the public caller accumulated one flattened response per call"
        );
    }

    #[test]
    fn expression_temporaries_reuse_their_emitted_locals() {
        let values = std::iter::repeat_n("tx-sender", 200)
            .collect::<Vec<_>>()
            .join(" ");
        let source = format!(
            "(define-read-only (f) (begin {values}))
             (f)"
        );
        crosscheck_compare_only(&source);
        assert!(
            emitted_function_locals(&source, "expression-temporaries", "f") < 100,
            "a repeated native principal allocated one offset local per expression"
        );
    }

    #[test]
    fn more_wasm_returns_than_wasmtime_allows_uses_packed_abi_and_loads() {
        // A return value flattens to one wasm result per leaf value: one
        // 600-field tuple is 1,200. User functions beyond wasmparser's
        // 1,000-result limit use the memory-backed ABI instead.
        let fields = (0..600_u32)
            .map(|index| format!("f{index}: 1"))
            .collect::<Vec<_>>()
            .join(", ");
        let snippet = format!("(define-public (f) (ok {{{fields}}}))");
        let mut compiled = compile(
            &snippet,
            &QualifiedContractIdentifier::new(
                StandardPrincipalData::transient(),
                ContractName::from_literal("wide-return"),
            ),
            LimitedCostTracker::new_free(),
            ClarityVersion::latest(),
            StacksEpochId::latest(),
            &mut AnalysisDatabase::new(&mut MemoryBackingStore::new()),
            true,
        )
        .expect("a function returning a wide tuple still compiles");

        wasmtime::Module::new(
            &crate::consensus_engine().expect("the consensus engine"),
            compiled.module.emit_wasm(),
        )
        .expect("wasmtime loads the packed-ABI module");
    }

    #[test]
    fn arity_report_measures_exact_and_packed_nested_boundaries() {
        fn tuple(ints: u32, with_bool: bool, values: bool) -> String {
            let mut fields = (0..ints)
                .map(|index| {
                    if values {
                        format!("f{index}: {index}")
                    } else {
                        format!("f{index}: int")
                    }
                })
                .collect::<Vec<_>>();
            if with_bool {
                fields.push(if values {
                    "flag: true".to_owned()
                } else {
                    "flag: bool".to_owned()
                });
            }
            fields.join(", ")
        }

        for (
            name,
            boundary,
            tuple_ints,
            tuple_bool,
            optional_ints,
            optional_bool,
            ok_ints,
            ok_bool,
        ) in [
            ("exact", 1_000, 500, false, 499, true, 498, true),
            ("packed", 1_001, 500, true, 500, false, 499, false),
        ] {
            let tuple_type = tuple(tuple_ints, tuple_bool, false);
            let tuple_value = tuple(tuple_ints, tuple_bool, true);
            let optional_value = tuple(optional_ints, optional_bool, true);
            let response_value = tuple(ok_ints, ok_bool, true);
            let source = format!(
                r#"
                (define-read-only (tuple-result) {{{tuple_value}}})
                (define-read-only (optional-result) (some {{{optional_value}}}))
                (define-read-only (tuple-param (value {{{tuple_type}}})) value)
                (define-public (response-result)
                    (if true (ok {{{response_value}}}) (err 0)))
                {{{tuple_value}}}
                "#
            );
            let mut compiled = compile(
                &source,
                &QualifiedContractIdentifier::new(
                    StandardPrincipalData::transient(),
                    ContractName::try_from(format!("arity-{name}"))
                        .expect("generated contract name is valid"),
                ),
                LimitedCostTracker::new_free(),
                ClarityVersion::latest(),
                StacksEpochId::latest(),
                &mut AnalysisDatabase::new(&mut MemoryBackingStore::new()),
                true,
            )
            .unwrap_or_else(|error| panic!("{name} boundary contract compiles: {error:?}"));

            assert_eq!(
                compiled.arity_report.max_function_params, boundary,
                "{name}"
            );
            assert_eq!(
                compiled.arity_report.max_function_results, boundary,
                "{name}"
            );
            assert_eq!(
                compiled.arity_report.max_control_results, boundary,
                "{name}"
            );
            assert_eq!(compiled.arity_report.top_level_results, boundary, "{name}");
            wasmtime::Module::new(
                &crate::consensus_engine().expect("the consensus engine"),
                compiled.module.emit_wasm(),
            )
            .unwrap_or_else(|error| panic!("{name} boundary module loads: {error}"));
        }
    }

    #[test]
    fn end_of_standard_data_is_correct() {
        let standard_lib_wasm: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/standard.wasm"));
        let module = Module::from_buffer(standard_lib_wasm).unwrap();
        let initial_data_size: usize = module.data.iter().map(|d| d.value.len()).sum();

        assert!((initial_data_size as u32) == END_OF_STANDARD_DATA);
    }

    /// Reads counted per `let`/`match` binding, in the order the bindings
    /// are introduced.
    fn binding_use_counts(snippet: &str) -> Vec<u32> {
        let ast = clarity::vm::ast::build_ast(
            &QualifiedContractIdentifier::new(
                StandardPrincipalData::transient(),
                ContractName::from_literal("binding-uses"),
            ),
            snippet,
            &mut LimitedCostTracker::new_free(),
            ClarityVersion::latest(),
            StacksEpochId::latest(),
        )
        .expect("test source parses");
        // A parsed-but-not-analyzed AST has no types; use counts and spill
        // marks do not need them.
        super::BindingUses::compute(&ast.expressions, |_| None).uses
    }

    #[test]
    fn binding_uses_let() {
        // Both bindings are read once in the body.
        assert_eq!(binding_use_counts("(let ((a u1) (b u2)) (+ a b))"), [1, 1]);
        // An unread binding counts zero.
        assert_eq!(binding_use_counts("(let ((a u1)) u2)"), [0]);
        // A binding is visible to the values bound after it.
        assert_eq!(binding_use_counts("(let ((a u1) (b a)) b)"), [1, 1]);
        // A shadowed binding keeps its own count: the outer `a` is unread,
        // the inner one is read once.
        assert_eq!(
            binding_use_counts("(let ((a u1)) (let ((a u2)) a))"),
            [0, 1]
        );
        // Uses after the scope closes resolve to the outer binding.
        assert_eq!(
            binding_use_counts("(let ((a u1)) (let ((b a)) b) a)"),
            [2, 1]
        );
    }

    #[test]
    fn binding_uses_match() {
        assert_eq!(binding_use_counts("(match (some u1) x (+ x u1) u0)"), [1]);
        // Each arm's binding is counted separately.
        assert_eq!(
            binding_use_counts("(match (ok u1) ok-v ok-v err-v (+ err-v err-v))"),
            [1, 2]
        );
        // An arm binding that shadows an enclosing `let` keeps its own count.
        assert_eq!(
            binding_use_counts("(let ((x u9)) (match (some u1) x x u0))"),
            [0, 1]
        );
    }

    /// A list that does not begin with a word still reads bindings. The
    /// allowance list of `as-contract?`/`restrict-assets?` is that shape, and a
    /// read counted zero times frees a slot the read still needs.
    #[test]
    fn binding_uses_counts_reads_under_a_headless_list() {
        assert_eq!(
            binding_use_counts(
                "(define-public (f (amount uint))
                   (let ((total (+ amount u1)))
                     (as-contract? ((with-stx total)) (ok true))))"
            ),
            [1]
        );
        assert_eq!(
            binding_use_counts(
                "(define-public (f (amount uint))
                   (let ((total (+ amount u1)))
                     (as-contract? ((with-ft current-contract \"t\" total)
                                    (with-stx total))
                       (ok true))))"
            ),
            [2]
        );
    }

    /// The mainnet shape: a *principal* binding read outside the allowance and
    /// again inside it.
    ///
    /// `SP1A27KFY4XERQCCRCARCYD1CC5N7M6688BSYADJ7.v0-5-market::supply-collateral-add`
    /// binds `ft-address` to `(contract-of ft)` and reads it three times — once
    /// in a later binding's value, once in an `is-eq`, and once inside
    /// `((with-ft ft-address "*" amount))`. Counted twice instead of three
    /// times, the second read frees the binding's locals and the branch after it
    /// borrows them back, so the allowance reads a principal from whatever offset
    /// now sits in that slot. Both halves are `i32`, so wasmtime has nothing to
    /// object to and the module loads: the failure is at run time, and it is
    /// `Unexpected principal data` — a version byte of 32 or more. That is what
    /// failed mainnet block 8,708,126, transaction `823f248a…`.
    ///
    /// The count is what the release depends on, so it is asserted rather than
    /// the symptom: an undercount that happens to land on a compatible slot
    /// computes with a principal nobody put there and no engine complains.
    #[test]
    fn binding_uses_counts_a_principal_read_from_an_allowance() {
        assert_eq!(
            binding_use_counts(
                "(define-trait ft-trait ((transfer (uint) (response bool uint))))
                 (define-public (supply-collateral-add (ft <ft-trait>) (amount uint))
                   (let ((ft-address (contract-of ft))
                         (asset (unwrap-panic (get-asset ft-address))))
                     (if (is-eq ft-address WRAPPER)
                       (as-contract? ((with-stx amount)) (ok asset))
                       (as-contract? ((with-ft ft-address \"*\" amount)) (ok asset)))))"
            ),
            [3, 2]
        );
    }

    #[test]
    fn binding_uses_ignores_parameters() {
        // The parameter `a` is not a `let`/`match` binding: only `b` is
        // counted.
        assert_eq!(
            binding_use_counts("(define-private (f (a uint)) (let ((b a)) (+ b a)))"),
            [1]
        );
    }

    #[test]
    fn function_argument_have_correct_type() {
        let snippet = r#"
            (define-private (foo (arg (optional uint)))
                true
            )

            (foo none)
        "#;
        crosscheck(snippet, Ok(Some(clarity::vm::Value::Bool(true))));

        // issue 340 showed a bug for epoch < 2.1
        assert!(crate::tools::evaluate_at(
            snippet,
            clarity::types::StacksEpochId::Epoch20,
            clarity::vm::version::ClarityVersion::Clarity1,
        )
        .is_ok());
    }

    #[test]
    fn local_call_widens_a_nested_none_argument() {
        crosscheck(
            r#"
            (define-private (reward-cycle (key {
                bond-index: (optional uint),
                reward-cycle: uint,
            }))
                (get reward-cycle key)
            )
            (let ((key {
                bond-index: none,
                reward-cycle: u7,
            }))
                (reward-cycle key)
            )
            "#,
            Ok(Some(clarity::vm::Value::UInt(7))),
        );
    }

    #[test]
    fn local_call_widens_a_constant_nested_none_argument() {
        crosscheck(
            r#"
            (define-constant stored-key {
                bond-index: none,
                reward-cycle: u7,
            })
            (define-private (reward-cycle (entry {
                bond-index: (optional uint),
                reward-cycle: uint,
            }))
                (get reward-cycle entry)
            )
            (reward-cycle stored-key)
            "#,
            Ok(Some(clarity::vm::Value::UInt(7))),
        );
    }

    #[test]
    fn local_call_widens_a_function_result_nested_none_argument() {
        crosscheck(
            r#"
            (define-private (stored-key)
                {
                    bond-index: none,
                    reward-cycle: u7,
                }
            )
            (define-private (reward-cycle (entry {
                bond-index: (optional uint),
                reward-cycle: uint,
            }))
                (get reward-cycle entry)
            )
            (reward-cycle (stored-key))
            "#,
            Ok(Some(clarity::vm::Value::UInt(7))),
        );
    }

    #[test]
    fn top_level_result_none() {
        crosscheck(
            "
(define-public (foo)
  (ok true))

(define-public (bar)
  (ok true))
",
            Ok(None),
        );
    }

    #[test]
    fn top_level_result_some_last() {
        crosscheck(
            "
(define-private (foo) 42)
(define-public (bar)
  (ok true))
(foo)
",
            evaluate("42"),
        );
    }

    #[test]
    fn top_level_result_some_not_last() {
        crosscheck(
            "
(define-public (foo)
  (ok true))
(foo)
(define-public (bar)
  (ok true))
",
            evaluate("(ok true)"),
        );
    }

    #[test]
    fn function_has_correct_argument_count() {
        // TODO: see issue #488
        // The inconsistency in function arguments should have been caught by the typechecker.
        // The runtime error below is being used as a workaround for a typechecker issue
        // where certain errors are not properly handled.
        // This test should be re-worked once the typechecker is fixed
        // and can correctly detect all argument inconsistencies.
        crosscheck(
            "
(define-public (foo (arg int))
  (ok true))
(foo 1 2)
(define-public (bar (arg int))
  (ok true))
(bar)
",
            Err(VmExecutionError::RuntimeCheck(
                RuntimeCheckErrorKind::IncorrectArgumentCount(1, 2),
            )),
        );
    }

    #[test]
    fn function_result_dont_erase_previous() {
        // from issue #475
        let snippet = r#"
        (define-map mymap int int)
        (define-private (somefn)
            (begin
                (map-set mymap 0 99)
                (err (list u"foo"))
            )
        )
        { fn: (somefn), mymap: (map-get? mymap 0) }
        "#;

        let expected = Value::from(
            TupleData::from_data(vec![
                (
                    ClarityName::from_literal("fn"),
                    Value::error(
                        Value::cons_list_unsanitized(vec![Value::string_utf8_from_bytes(
                            b"foo".to_vec(),
                        )
                        .unwrap()])
                        .unwrap(),
                    )
                    .unwrap(),
                ),
                (
                    ClarityName::from_literal("mymap"),
                    Value::some(Value::Int(99)).unwrap(),
                ),
            ])
            .unwrap(),
        );

        crosscheck(snippet, Ok(Some(expected)));
    }

    #[test]
    fn function_call_needs_ducktyping() {
        let snippet = r#"
            (define-public (execute)
                (if true (foo) (err u42))
            )

            (define-private (foo)
                (ok u123)
            )

            (execute)
    "#;

        crosscheck(snippet, Ok(Some(Value::okay(Value::UInt(123)).unwrap())));
    }

    //
    // Module with tests that should only be executed
    // when running Clarity::V2 or Clarity::v3.
    //
    #[cfg(not(feature = "test-clarity-v1"))]
    #[cfg(test)]
    mod clarity_v2_v3 {
        use super::*;

        #[test]
        fn is_in_mainnet() {
            crosscheck(
                "
    (define-public (mainnet)
      (ok is-in-mainnet))

    (mainnet)
    ",
                evaluate("(ok false)"),
            );
        }
    }

    #[cfg(feature = "test-clarity-v1")]
    #[test]
    fn static_contract_call_has_right_type_set() {
        let callee = (
            ContractName::from_literal("callee"),
            "(define-read-only (print-param (par {x: (optional uint),y: int,}))
                        (print par))",
        );
        let caller = (
            ContractName::from_literal("caller"),
            "(contract-call? .callee print-param (tuple (x none) (y -54756928044990108781631836)))",
        );

        let expected = Ok(Some(Value::from(
            TupleData::from_data(vec![
                (ClarityName::from_literal("x"), Value::none()),
                (
                    ClarityName::from_literal("y"),
                    Value::Int(-54756928044990108781631836),
                ),
            ])
            .unwrap(),
        )));
        crate::tools::crosscheck_multi_contract(&[callee, caller], expected);
    }

    #[cfg(feature = "test-clarity-v1")]
    #[test]
    fn dynamic_contract_call_has_right_type_set() {
        let callee = (
            ContractName::from_literal("callee"),
            "(define-trait printer
                        ((print-param ({x: (optional uint), y: int})
                                      (response {x: (optional uint), y: int} uint))))
                     (define-public (print-param (par {x: (optional uint), y: int}))
                        (ok (print par)))",
        );
        let caller = (
            ContractName::from_literal("caller"),
            "(use-trait printer .callee.printer)
                     (define-private (call-it (tt <printer>))
                        (contract-call? tt print-param (tuple (x none) (y -54756928044990108781631836))))
                     (call-it .callee)",
        );
        let expected = Ok(Some(
            Value::okay(Value::from(
                TupleData::from_data(vec![
                    (ClarityName::from_literal("x"), Value::none()),
                    (
                        ClarityName::from_literal("y"),
                        Value::Int(-54756928044990108781631836),
                    ),
                ])
                .unwrap(),
            ))
            .unwrap(),
        ));
        crate::tools::crosscheck_multi_contract(&[callee, caller], expected);
    }
}
