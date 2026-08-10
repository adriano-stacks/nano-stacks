use std::fmt::Write as _;

use blockstack_lib::chainstate::stacks::db::StacksChainState;
use blockstack_lib::chainstate::stacks::db::testing::TestChainstateBuilder;
use blockstack_lib::chainstate::stacks::{
    MAX_TRANSACTION_LEN, StacksTransaction, StacksTransactionSigner, TransactionAuth,
    TransactionPayload, TransactionPostConditionMode, TransactionVersion,
};
use blockstack_lib::core::{BLOCK_LIMIT_MAINNET_40, CHAIN_ID_TESTNET};
use clarity::vm::clarity::ClarityConnection;
use clarity::vm::representations::ContractName;
use clarity::vm::test_util::generate_test_burn_state_db;
use clarity::vm::types::{QualifiedContractIdentifier, Value};
use stacks_common::codec::StacksMessageCodec;
use stacks_common::consts::{FIRST_BURNCHAIN_CONSENSUS_HASH, FIRST_STACKS_BLOCK_HASH};
use stacks_common::types::StacksEpochId;
use stacks_common::types::chainstate::{BlockHeaderHash, ConsensusHash, StacksPrivateKey};

const CONTRACT_NAME: &str = "arity-boundary";
const TUPLE_FIELDS: usize = 501;

fn over_arity_contract() -> String {
    let mut fields = String::new();
    for index in 0..TUPLE_FIELDS {
        if !fields.is_empty() {
            fields.push_str(", ");
        }
        write!(&mut fields, "field-{index:03}: uint")
            .expect("writing a tuple field to a String cannot fail");
    }
    format!("(define-read-only (roundtrip (value {{{fields}}})) value)")
}

#[test]
fn the_pinned_stock_node_deploys_a_function_beyond_wasmtime_arity() {
    let source = over_arity_contract();
    let transaction_limit =
        usize::try_from(MAX_TRANSACTION_LEN).expect("the transaction limit fits usize");
    assert_eq!(TUPLE_FIELDS * 2, 1_002);
    assert!(source.len() < transaction_limit);

    let private_key = StacksPrivateKey::from_hex(
        "6d430bb91222408e7706c9001cfaeb91b08c2be6d5ac95779ab52c6b431950e001",
    )
    .expect("a fixed test key");
    let auth = TransactionAuth::from_p2pkh(&private_key).expect("single-signature auth");
    let address = auth.origin().address_testnet();
    let contract_name = ContractName::try_from(CONTRACT_NAME).expect("a contract name");
    let contract_id = QualifiedContractIdentifier::new(address.clone().into(), contract_name);
    let payload = TransactionPayload::new_smart_contract(CONTRACT_NAME, &source, None)
        .expect("a consensus-encodable contract source");
    let mut transaction = StacksTransaction::new(TransactionVersion::Testnet, auth, payload);
    transaction.chain_id = CHAIN_ID_TESTNET;
    transaction.post_condition_mode = TransactionPostConditionMode::Allow;
    transaction.set_tx_fee(0);

    let mut signer = StacksTransactionSigner::new(&transaction);
    signer
        .sign_origin(&private_key)
        .expect("sign the deployment");
    let signed_deployment = signer.get_tx().expect("a signed deployment");
    let encoded = signed_deployment.serialize_to_vec();
    assert!(encoded.len() <= transaction_limit);

    let mut chainstate = TestChainstateBuilder::new_testnet(
        "the-pinned-stock-node-deploys-a-function-beyond-wasmtime-arity",
    )
    .with_balances(vec![(address, 1_000_000)])
    .build();
    let burn_state = generate_test_burn_state_db(StacksEpochId::Epoch40);
    let deployed_consensus = ConsensusHash([1; 20]);
    let deployed_block = BlockHeaderHash([1; 32]);
    let mut connection = chainstate.block_begin(
        &burn_state,
        &FIRST_BURNCHAIN_CONSENSUS_HASH,
        &FIRST_STACKS_BLOCK_HASH,
        &deployed_consensus,
        &deployed_block,
    );
    let (fee, receipt) =
        StacksChainState::process_transaction(&mut connection, &signed_deployment, false, None)
            .expect("pinned stacks-core deploys the contract");
    assert_eq!(fee, 0);
    assert_eq!(receipt.result, Value::okay_true());
    assert!(receipt.contract_analysis.is_some());
    assert!(!receipt.execution_cost.exceeds(&BLOCK_LIMIT_MAINNET_40));
    assert!(!receipt.post_condition_aborted);
    assert!(receipt.vm_error.is_none());
    assert!(connection.with_analysis_db_readonly(|db| db.has_contract(&contract_id)));
    assert_eq!(
        connection.with_clarity_db_readonly(|db| db.get_contract_src(&contract_id)),
        Some(source)
    );
    connection.commit_to_block(&deployed_consensus, &deployed_block);
}
