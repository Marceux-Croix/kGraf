//! A caller reaches for `Default`, which is the accidental way this guarantee is
//! most likely to be lost: adding `#[derive(Default)]` to a struct is a reflex,
//! and here it would open the door.

use kgraf_provider::CallToken;

fn main() {
    let _token: CallToken<'static> = Default::default();
}
