//! List candidate rigs (no ID queries). Usage: cargo run --example scan
fn main() {
    for d in term73::devices::scan() {
        println!("{:>6} {:?} {:?} serial={}", d.model.clone().unwrap_or_default(), d.target, d.name, d.serial);
    }
}
