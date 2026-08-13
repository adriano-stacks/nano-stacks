use clarity::vm::{ClarityName, SymbolicExpression, SymbolicExpressionType};

use super::{ComplexWord, Word};
use crate::check_args;
use crate::cost::ChargeGenerator;
use crate::wasm_generator::{ArgumentsExt, GeneratorError, WasmGenerator};
use crate::wasm_utils::{check_argument_count, ArgumentCountCheck};

#[derive(Debug)]
pub struct DefineTrait;

impl Word for DefineTrait {
    fn name(&self) -> ClarityName {
        ClarityName::from_literal("define-trait")
    }
}

impl ComplexWord for DefineTrait {
    fn traverse(
        &self,
        generator: &mut WasmGenerator,
        builder: &mut walrus::InstrSeqBuilder,
        _expr: &SymbolicExpression,
        args: &[SymbolicExpression],
    ) -> Result<(), GeneratorError> {
        check_argument_count(generator, builder, 2, args.len(), ArgumentCountCheck::Exact)?;

        let name = args.get_name(0)?;
        // Making sure if name is not reserved
        if generator.is_reserved_name(name) {
            return Err(GeneratorError::InternalError(format!(
                "Name already used {name:?}"
            )));
        }

        let methods = args
            .get_expr(1)?
            .match_list()
            .ok_or_else(|| GeneratorError::TypeError("invalid trait definition".to_owned()))?;
        for method in methods {
            let signature = method
                .match_list()
                .ok_or_else(|| GeneratorError::TypeError("invalid trait method".to_owned()))?;
            let parameters = signature
                .get(1)
                .and_then(SymbolicExpression::match_list)
                .ok_or_else(|| GeneratorError::TypeError("invalid trait parameters".to_owned()))?;
            for parameter in parameters {
                generator.charge_type_parse(builder, parameter)?;
            }
            let return_type = signature
                .get(2)
                .ok_or_else(|| GeneratorError::TypeError("missing trait return type".to_owned()))?;
            generator.charge_type_parse(builder, return_type)?;
        }

        // Store the identifier as a string literal in the memory
        let (name_offset, name_length) = generator.add_string_literal(name)?;

        // Push the name onto the data stack
        builder
            .i32_const(name_offset as i32)
            .i32_const(name_length as i32);

        builder.call(
            generator
                .module
                .funcs
                .by_name("stdlib.define_trait")
                .ok_or_else(|| {
                    GeneratorError::InternalError("stdlib.define_trait not found".to_owned())
                })?,
        );
        Ok(())
    }
}

#[derive(Debug)]
pub struct UseTrait;

impl Word for UseTrait {
    fn name(&self) -> ClarityName {
        ClarityName::from_literal("use-trait")
    }
}

impl ComplexWord for UseTrait {
    fn traverse(
        &self,
        generator: &mut WasmGenerator,
        builder: &mut walrus::InstrSeqBuilder,
        _expr: &SymbolicExpression,
        args: &[SymbolicExpression],
    ) -> Result<(), GeneratorError> {
        check_argument_count(generator, builder, 2, args.len(), ArgumentCountCheck::Exact)?;

        // We simply add the trait to the memory so that contract-call?
        // can retrieve a correct function return type at call.
        let trait_id = args
            .get_expr(1)?
            .match_field()
            .ok_or_else(|| {
                GeneratorError::TypeError(
                    "use-trait second argument should be the imported trait".to_owned(),
                )
            })?
            .clone();

        let offset_len = generator.add_trait_identifier(&trait_id)?;
        generator.used_traits.insert(trait_id, offset_len);

        Ok(())
    }
}

#[derive(Debug)]
pub struct ImplTrait;

impl Word for ImplTrait {
    fn name(&self) -> ClarityName {
        ClarityName::from_literal("impl-trait")
    }
}

impl ComplexWord for ImplTrait {
    fn traverse(
        &self,
        generator: &mut WasmGenerator,
        builder: &mut walrus::InstrSeqBuilder,
        _expr: &SymbolicExpression,
        args: &[SymbolicExpression],
    ) -> Result<(), GeneratorError> {
        check_args!(generator, builder, 1, args.len(), ArgumentCountCheck::Exact);

        let trait_identifier = match &args.get_expr(0)?.expr {
            SymbolicExpressionType::Field(trait_identifier) => trait_identifier,
            _ => {
                return Err(GeneratorError::TypeError(
                    "Expected trait identifier".into(),
                ))
            }
        };

        // Store the trait identifier as a string literal in the memory
        let (trait_offset, trait_length) =
            generator.add_string_literal(&trait_identifier.to_string())?;

        // Push the name onto the data stack
        builder
            .i32_const(trait_offset as i32)
            .i32_const(trait_length as i32);

        builder.call(
            generator
                .module
                .funcs
                .by_name("stdlib.impl_trait")
                .ok_or_else(|| {
                    GeneratorError::InternalError("stdlib.impl_trait not found".to_owned())
                })?,
        );
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use clarity::types::StacksEpochId;
    use clarity::vm::types::{
        CallableData, QualifiedContractIdentifier, StandardPrincipalData, TraitIdentifier,
    };
    use clarity::vm::{ClarityName, ClarityVersion, ContractName, Value};

    use crate::tools::{
        crosscheck, crosscheck_expect_failure, crosscheck_multi_contract,
        crosscheck_multi_contract_with_env, TestEnvironment,
    };

    //
    // Module with tests that should only be executed
    // when running Clarity::V1.
    //
    #[cfg(feature = "test-clarity-v1")]
    mod clarity_v1 {
        use super::*;
        use crate::tools::crosscheck_with_epoch;

        #[test]
        fn validate_define_trait_epoch() {
            // Epoch20
            crosscheck_with_epoch(
                "(define-trait index-of? ((func (int) (response int int))))",
                Ok(None),
                StacksEpochId::Epoch20,
            );

            crosscheck_expect_failure("(define-trait index-of? ((func (int) (response int int))))");
        }
    }

    #[test]
    fn define_trait_eval() {
        // Just validate that it doesn't crash
        crosscheck("(define-trait my-trait ())", Ok(None))
    }

    #[test]
    fn define_trait_check_context() {
        let mut env = TestEnvironment::default();
        let val = env
            .init_contract_with_snippet(
                "token-trait",
                r#"
(define-trait token-trait
    ((transfer? (principal principal uint) (response uint uint))
        (get-balance (principal) (response uint uint))))
             "#,
            )
            .unwrap();

        assert!(val.is_none());
        let contract_context = env.get_contract_context("token-trait").unwrap();
        let token_trait = contract_context
            .lookup_trait_definition("token-trait")
            .unwrap();
        assert_eq!(token_trait.len(), 2);
    }

    #[test]
    fn use_trait_eval() {
        let mut env = TestEnvironment::default();
        env.init_contract_with_snippet(
            "my-trait",
            r#"
(define-trait my-trait
    ((add (int int) (response int int))))
             "#,
        )
        .expect("Failed to init contract.");
        let val = env
            .init_contract_with_snippet("use-token", "(use-trait the-trait .my-trait.my-trait)")
            .expect("Failed to init contract.");

        assert!(val.is_none());
    }

    #[test]
    fn use_trait_call() {
        let mut env = TestEnvironment::default();
        env.init_contract_with_snippet(
            "my-trait",
            r#"
(define-trait my-trait
  ((add (int int) (response int int))))
(define-public (add (a int) (b int))
  (ok (+ a b))
)
            "#,
        )
        .expect("Failed to init contract.");
        let val = env
            .init_contract_with_snippet(
                "use-trait",
                r#"
(use-trait the-trait .my-trait.my-trait)
(define-private (foo (adder <the-trait>) (a int) (b int))
    (contract-call? adder add a b)
)
(foo .my-trait 1 2)
            "#,
            )
            .expect("Failed to init contract.");

        assert_eq!(val.unwrap(), Value::okay(Value::Int(3)).unwrap());
    }

    #[test]
    fn impl_trait_eval() {
        let mut env = TestEnvironment::default();
        env.init_contract_with_snippet(
            "my-trait",
            r#"
(define-trait my-trait
  ((add (int int) (response int int))))
            "#,
        )
        .expect("Failed to init contract.");
        let val = env
            .init_contract_with_snippet(
                "impl-trait",
                r#"
(impl-trait .my-trait.my-trait)
(define-public (add (a int) (b int))
  (ok (+ a b))
)
            "#,
            )
            .expect("Failed to init contract.");

        assert!(val.is_none());

        let contract_context = env.get_contract_context("impl-trait").unwrap();
        assert!(contract_context
            .implemented_traits
            .contains(&TraitIdentifier::new(
                StandardPrincipalData::transient(),
                ContractName::from_literal("my-trait"),
                ClarityName::from_literal("my-trait"),
            )));
    }

    #[test]
    fn trait_list() {
        // NOTE: this also tests `print` of `Callable`
        let first_contract_name = ContractName::from_literal("my-trait-contract");
        let first_snippet = r#"
(define-trait my-trait
  ((add (int int) (response int int))))
(define-public (add (a int) (b int))
  (ok (+ a b))
)
            "#;

        let second_contract_name = ContractName::from_literal("use-trait");
        let second_snippet = r#"
(use-trait the-trait .my-trait-contract.my-trait)
(define-private (foo (adder <the-trait>))
    (print (list adder adder))
)
(foo .my-trait-contract)
            "#;

        let contract_id = QualifiedContractIdentifier {
            issuer: StandardPrincipalData::transient(),
            name: ContractName::from_literal("my-trait-contract"),
        };
        crosscheck_multi_contract(
            &[
                (first_contract_name, first_snippet),
                (second_contract_name, second_snippet),
            ],
            Ok(Some(
                Value::cons_list(
                    (0..2)
                        .map(|_| {
                            Value::CallableContract(CallableData {
                                contract_identifier: contract_id.clone(),
                                trait_identifier: Some(Box::new(TraitIdentifier {
                                    name: ClarityName::from_literal("my-trait"),
                                    contract_identifier: contract_id.clone(),
                                })),
                            })
                        })
                        .collect(),
                    &StacksEpochId::latest(),
                )
                .unwrap(),
            )),
        );
    }

    /// The same `print` of a trait reference, under the epoch-2.05 type checker.
    ///
    /// This is mainnet 8,707,847, and the epoch is the whole of it.
    /// `SPNWZ5V2TPWGQGVDR6T7B6RQ4XMGZ4PXTEE0VQ0S.marketplace-bid-v5` — Clarity 1,
    /// analysed in **Epoch 2.05** — has `(print { collection_id: collection, … })`
    /// where `collection` is a `<nft-trait>` parameter. The 2.05 type checker
    /// types such a parameter `TraitReferenceType`; 2.1's types it
    /// `CallableType(CallableSubtype::Trait(_))`, and only the second spelling
    /// was in `type_for_serialization`'s mapping to `PrincipalType`. So the type
    /// clar2wasm wrote into literal memory for `print` to read back was
    /// `(tuple (collection_id <SP2PAB….nft-trait.nft-trait>))`, and `<…>` holding
    /// a *qualified* trait identifier is not Clarity anybody can parse — the
    /// angle brackets take a local alias `use-trait` introduced. The round-trip
    /// check in `serialized_type_of` therefore failed the build, at a *call*,
    /// which is nano's gap and not the transaction's, so the node refused the
    /// block and replay stopped.
    ///
    /// `trait_list` above covers the same `print` at the latest epoch and passes
    /// either way, which is why the gap survived: nothing asked the question in
    /// the epoch 146,000 mainnet contracts were analysed in.
    #[test]
    fn print_a_trait_reference_under_the_two_oh_five_type_checker() {
        let trait_contract = ContractName::from_literal("my-trait-contract");
        let trait_snippet = r#"
(define-trait my-trait
  ((add (int int) (response int int))))
(define-public (add (a int) (b int))
  (ok (+ a b))
)
            "#;

        // A trait reference on its own, in a tuple, and in a list: the tuple is
        // the mainnet shape, and the other two are the same type reached through
        // `type_for_serialization`'s other recursive arms.
        let printing = ContractName::from_literal("use-trait");
        let printing_snippet = r#"
(use-trait the-trait .my-trait-contract.my-trait)
(define-private (foo (adder <the-trait>))
  (begin
    (print adder)
    (print (list adder))
    (print { collection_id: adder, id: u1 })))
(foo .my-trait-contract)
            "#;

        // The value the reference implementation prints, and the reason the fix
        // is `PrincipalType` rather than some other stand-in: at 2.05 a trait
        // reference *is* a contract principal — no `CallableContract`, and the
        // tuple's own field type is `principal`.
        let deployer = QualifiedContractIdentifier {
            issuer: StandardPrincipalData::transient(),
            name: ContractName::from_literal("my-trait-contract"),
        };
        let printed = Value::Tuple(
            clarity::vm::types::TupleData::from_data(vec![
                (
                    ClarityName::from_literal("collection_id"),
                    Value::Principal(clarity::vm::types::PrincipalData::Contract(deployer)),
                ),
                (ClarityName::from_literal("id"), Value::UInt(1)),
            ])
            .expect("a tuple"),
        );

        // Epoch 2.05 pairs with Clarity 1 (`epoch_and_clarity_match`), so this
        // is the contract's own pairing rather than one the harness rewrote.
        crosscheck_multi_contract_with_env(
            &[
                (trait_contract, trait_snippet),
                (printing, printing_snippet),
            ],
            Ok(Some(printed)),
            TestEnvironment::new(StacksEpochId::Epoch2_05, ClarityVersion::Clarity1),
        );
    }

    #[test]
    fn validate_define_trait() {
        // Reserved keyword
        crosscheck_expect_failure("(define-trait map ((func (int) (response int int))))");

        // Custom trait token name
        crosscheck(
            "(define-trait a ((func (int) (response int int))))",
            Ok(None),
        );

        // Custom trait name duplicate
        let snippet = r#"
          (define-trait a ((func (int) (response int int))))
          (define-trait a ((func (int) (response int int))))
        "#;
        crosscheck_expect_failure(snippet);
    }
}
