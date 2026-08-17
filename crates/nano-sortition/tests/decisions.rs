use nano_bitcoin::BitcoinBlock;
use nano_sortition::snapshot_for;

#[test]
fn a_seed_snapshot_carries_the_bitcoin_timestamp() {
    let snapshot = snapshot_for(&BitcoinBlock {
        timestamp: 1_787_000_001,
        height: 42,
        hash: [7; 32],
        operations: Vec::new(),
    });

    assert_eq!(snapshot.bitcoin_timestamp, 1_787_000_001);
}
