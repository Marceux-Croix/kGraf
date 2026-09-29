use fx_interface_greeter::Greeter;

struct Loud;

impl Greeter for Loud {
    fn greet(&self, name: &str) -> String {
        format!("HELLO, {}!", name.to_uppercase())
    }
}

#[test]
fn a_consumer_implements_the_declared_trait() {
    let g = Loud;
    assert_eq!(g.greet("ada"), "HELLO, ADA!");
}
