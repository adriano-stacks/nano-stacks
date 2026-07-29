use clarity::vm::types::signatures::TypeSignature;

pub const PRINCIPAL_BYTES: usize = 21;
pub const STANDARD_PRINCIPAL_BYTES: usize = 22;

pub fn get_type_size(ty: &TypeSignature) -> i32 {
    match ty {
        TypeSignature::IntType | TypeSignature::UIntType => 16,
        TypeSignature::BoolType => 4,
        TypeSignature::PrincipalType
        | TypeSignature::SequenceType(_)
        | TypeSignature::CallableType(_)
        | TypeSignature::ListUnionType(_)
        | TypeSignature::TraitReferenceType(_) => 8,
        TypeSignature::OptionalType(inner) => 4 + get_type_size(inner),
        TypeSignature::TupleType(tuple_ty) => {
            tuple_ty.get_type_map().values().map(get_type_size).sum()
        }
        TypeSignature::ResponseType(inner) => 4 + get_type_size(&inner.0) + get_type_size(&inner.1),
        TypeSignature::NoType => 4,
    }
}
