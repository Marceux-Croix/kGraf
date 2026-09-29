// An Interpretation is made only by the pass or a validated stored reading.
fn main() {
    let json = r#"{"plan_id":"plan-x","mode":"resume"}"#;
    let i: kgraf_interpret::Interpretation = serde_json::from_str(json).expect("json");
    let _ = i.approve("plan-x", &[]);
}
