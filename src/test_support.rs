//! Test-only helpers.

use std::cell::Cell;
use std::sync::Once;

thread_local! {
    static PANICKED: Cell<bool> = const { Cell::new(false) };
}

/// Rebuild a test `VirtualDom` and fail the test if anything panicked while
/// it rendered. Dioxus catches a component's panic and renders an error
/// instead, so an assertion inside `VirtualDom::new(|| { .. })` that fails
/// would otherwise leave the test green. Use this in place of
/// `dom.rebuild_in_place()` in tests.
pub fn rebuild_or_fail(dom: &mut dioxus::prelude::VirtualDom) {
    static HOOK: Once = Once::new();
    HOOK.call_once(|| {
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            PANICKED.with(|p| p.set(true));
            previous(info);
        }));
    });
    PANICKED.with(|p| p.set(false));
    dom.rebuild_in_place();
    assert!(
        !PANICKED.with(|p| p.get()),
        "a panic inside the Dioxus runtime (see the message above) failed this test"
    );
}
