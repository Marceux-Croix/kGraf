//! A caller copies one approval to run the call twice.

use kgraf_tools::approval::ApprovedCall;

fn twice(approved: ApprovedCall) -> (ApprovedCall, ApprovedCall) {
    (approved.clone(), approved)
}

fn main() {
    let _ = twice;
}
