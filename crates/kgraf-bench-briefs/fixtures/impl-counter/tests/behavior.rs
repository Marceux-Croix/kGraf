use fx_impl_counter::Counter;

#[test]
fn counts_up() {
    let mut c = Counter::new();
    assert_eq!(c.get(), 0);
    c.incr();
    c.incr();
    assert_eq!(c.get(), 2);
}
