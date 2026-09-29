//! A caller fabricates an approval, so no decision is ever recorded.

// The `unimplemented!()` placeholders are unreachable by construction; silencing
// the warning keeps the expected output to the one error under test.
#![allow(unreachable_code)]

use kgraf_tools::approval::ApprovedCall;

fn main() {
    let _approved = ApprovedCall {
        call_id: String::new(),
        tool: String::new(),
        input: unimplemented!(),
        access: unimplemented!(),
        rewritten: false,
    };
}
