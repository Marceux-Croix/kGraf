//! A caller tries to fabricate a token with a struct literal.
//!
//! This is the shortest route around the ledger and therefore the one a worker
//! optimizing a hot path would find first.

// The `unimplemented!()` placeholders are unreachable by construction; silencing
// the warning keeps the expected compiler output to the one error under test.
#![allow(unreachable_code)]

use kgraf_provider::CallToken;

fn main() {
    let _token = CallToken {
        ledger: unimplemented!(),
        id: unimplemented!(),
        settled: unimplemented!(),
        adapter: "sneaky",
        stopped: unimplemented!(),
        stop_probes: unimplemented!(),
    };
}
