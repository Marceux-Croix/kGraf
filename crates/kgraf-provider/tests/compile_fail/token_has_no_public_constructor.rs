//! A caller looks for the constructor that every other type in this workspace has.
//!
//! `CallToken` deliberately does not have one. If someone adds a `new`, a
//! `Default`, or a `From` for convenience, this fixture starts compiling and the
//! test goes red.

use kgraf_provider::CallToken;

fn main() {
    let _token = CallToken::new();
}
