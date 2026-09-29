//! A caller fabricates a permit with a struct literal.

use kgraf_tools::CallPermit;

fn main() {
    let _permit = CallPermit { _private: () };
}
