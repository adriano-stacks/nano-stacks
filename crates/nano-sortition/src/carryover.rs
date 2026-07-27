use nano_primitives::Uint256;

include!(concat!(env!("OUT_DIR"), "/carryover_lookup.rs"));

#[derive(Clone, Copy)]
struct FixedPoint(Uint256);

impl FixedPoint {
    const fn zero() -> Self {
        Self(Uint256::zero())
    }

    fn one() -> Self {
        Self(Uint256::one() << 64)
    }

    fn fraction(numerator: u64, denominator: u64) -> Self {
        Self((Uint256::from(numerator) << 64) / Uint256::from(denominator))
    }

    fn minimum(self, other: Self) -> Self {
        Self(self.0.min(other.0))
    }

    fn subtract(self, other: Self) -> Option<Self> {
        (self.0 >= other.0).then(|| Self(self.0 - other.0))
    }

    fn multiply(self, other: Self) -> Self {
        Self((self.0 * other.0) >> 64)
    }

    fn integer_part(self) -> u64 {
        (self.0 >> 64).low_u64()
    }

    fn probability(self) -> Uint256 {
        let one = Self::one();
        if self.0 >= one.0 {
            (one.0 - Uint256::one()) << 192
        } else {
            self.0 << 192
        }
    }
}

pub fn null_miner_probability(block_burn: u64, window_median_burn: u64) -> Option<Uint256> {
    let carryover = carryover(block_burn, window_median_burn)?;
    let advantage = lookup_advantage(carryover);
    let probability = carryover
        .multiply(advantage)
        .0
        .checked_add(FixedPoint::one().subtract(carryover)?.0)?;
    Some(
        FixedPoint(probability)
            .minimum(FixedPoint::one())
            .probability(),
    )
}

fn carryover(block_burn: u64, window_median_burn: u64) -> Option<FixedPoint> {
    if window_median_burn == 0 {
        return Some(FixedPoint::zero());
    }
    (block_burn < window_median_burn).then(|| FixedPoint::fraction(block_burn, window_median_burn))
}

fn lookup_advantage(carryover: FixedPoint) -> FixedPoint {
    let capped = carryover.minimum(FixedPoint::one());
    let index = usize::try_from(
        capped
            .multiply(FixedPoint::fraction(1024, 1))
            .integer_part(),
    )
    .expect("lookup index fits usize")
    .min(CARRYOVER_ADVANTAGE.len() - 1);
    FixedPoint(Uint256::from(CARRYOVER_ADVANTAGE[index]))
}

#[cfg(test)]
mod tests {
    use super::{CARRYOVER_ADVANTAGE, FixedPoint, null_miner_probability};
    use nano_primitives::sha256;

    #[test]
    fn lookup_uses_protocol_endpoints() {
        assert_eq!(CARRYOVER_ADVANTAGE.len(), 1024);
        assert_eq!(CARRYOVER_ADVANTAGE[0], 14_665_006_693_661_589_504);
        assert_eq!(CARRYOVER_ADVANTAGE[1023], 17_835_588_001_385_282);
        let bytes = CARRYOVER_ADVANTAGE
            .iter()
            .flat_map(|value| value.to_le_bytes())
            .collect::<Vec<_>>();
        assert_eq!(
            sha256(&bytes).to_string(),
            "298c4385dd66538bb320692a658dac218533c902f2f8b868ff5a59ad50900037"
        );
    }

    #[test]
    fn full_carryover_does_not_enable_the_null_miner() {
        assert_eq!(null_miner_probability(10, 10), None);
    }

    #[test]
    fn empty_carryover_gives_the_null_miner_the_whole_range() {
        assert_eq!(
            null_miner_probability(0, 10),
            Some((FixedPoint::one().0 - 1) << 192)
        );
    }
}
