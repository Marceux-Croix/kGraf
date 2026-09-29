//! A caller mints a permit to call a tool directly, skipping approval.

use kgraf_tools::CallPermit;

fn main() {
    let _permit = CallPermit::new();
}
