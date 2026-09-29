// An Interpretation's fields are private: no struct literal outside the crate.
fn main() {
    let _ = kgraf_interpret::Interpretation {
        plan_id: "plan-x".into(),
    };
}
