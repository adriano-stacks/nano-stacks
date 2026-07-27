use std::{env, fmt::Write, fs, path::PathBuf};

#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss,
    clippy::suboptimal_flops
)]
fn main() {
    let output = PathBuf::from(env::var_os("OUT_DIR").expect("Cargo sets OUT_DIR"))
        .join("carryover_lookup.rs");
    let mut source = String::from("const CARRYOVER_ADVANTAGE: [u64; 1024] = [\n");
    for index in 0..1024 {
        let carryover = f64::from(index) / 1024.0;
        let advantage = 0.8
            / (1.0
                + std::f64::consts::E
                    .powf(-11.795_830_089_282_052 * (0.429_576_908_162_046_47 - carryover)));
        let fixed_point = (advantage * ((u128::from(u64::MAX) + 1) as f64)) as u64;
        writeln!(source, "    {},", readable_integer(fixed_point)).expect("write lookup entry");
    }
    source.push_str("];\n");
    fs::write(output, source).expect("write carryover lookup table");
    println!("cargo:rerun-if-changed=build.rs");
}

fn readable_integer(value: u64) -> String {
    let decimal = value.to_string();
    let split = decimal.len() % 3;
    let mut result = String::with_capacity(decimal.len() + (decimal.len() - 1) / 3);
    if split != 0 {
        result.push_str(&decimal[..split]);
    }
    for chunk in decimal.as_bytes()[split..].chunks(3) {
        if !result.is_empty() {
            result.push('_');
        }
        result.push_str(std::str::from_utf8(chunk).expect("decimal digits are UTF-8"));
    }
    result
}
