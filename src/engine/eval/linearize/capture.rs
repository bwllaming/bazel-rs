//! What the engine's own services do while the evaluator runs (S1:
//! loading, registration patterns and package groups are coordinator
//! services on the engine), captured on the evaluation thread so the node
//! running them owns it in its transcript, in order: the incremental
//! tracker's scopes (`SharedScope`, `PatternScope`, `SuiteScope`) and the
//! target lookups the engine makes itself. The node bodies drain it into
//! the running attempt (`nodes::bodies::Coord::drain`).
//!
//! Off (one thread-local read per hook) unless [`start`] installed it.

use std::cell::{Cell, RefCell};

use crate::label::Label;

use super::super::transcript::Event;

thread_local! {
    static ON: Cell<bool> = const { Cell::new(false) };
    /// Record target lookups (the incremental tracker is on).
    static TRACKING: Cell<bool> = const { Cell::new(false) };
    static EVENTS: RefCell<Vec<Event>> = const { RefCell::new(Vec::new()) };
}

/// Capture on this thread until [`stop`].
pub(crate) fn start(tracking: bool) {
    EVENTS.with(|events| events.borrow_mut().clear());
    TRACKING.with(|cell| cell.set(tracking));
    ON.with(|on| on.set(true));
}

pub(crate) fn stop() {
    ON.with(|on| on.set(false));
    PAUSED.with(|cell| cell.set(false));
    EVENTS.with(|events| events.borrow_mut().clear());
}

thread_local! {
    static PAUSED: Cell<bool> = const { Cell::new(false) };
}

/// Set capturing aside (`true`) while the engine works as the walk (a
/// flush's phase 2), or take it up again (`false`).
pub(crate) fn pause(paused: bool) {
    PAUSED.with(|cell| cell.set(paused));
}

fn push(event: impl FnOnce() -> Event) {
    if ON.with(Cell::get) && !PAUSED.with(Cell::get) {
        EVENTS.with(|events| events.borrow_mut().push(event()));
    }
}

/// A tracker scope begins (`incremental::{Shared,Pattern,Suite}Scope`).
pub(crate) fn scope_enter(name: &'static str) {
    push(|| Event::ScopeEnter(name));
}

/// The innermost tracker scope ends.
pub(crate) fn scope_exit() {
    push(|| Event::ScopeExit);
}

/// The engine looks up `label` (`Engine::target`).
pub(crate) fn access(label: &Label) {
    if TRACKING.with(Cell::get) {
        push(|| Event::Access(label.clone()));
    }
}

/// What was captured since the last call.
pub(crate) fn take() -> Vec<Event> {
    EVENTS.with(|events| std::mem::take(&mut *events.borrow_mut()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn captures_only_while_on_and_accesses_only_while_tracking() {
        scope_enter("before");
        start(false);
        scope_enter("x");
        access(&Label::parse_canonical("//a:a").unwrap());
        scope_exit();
        assert_eq!(take(), [Event::ScopeEnter("x"), Event::ScopeExit]);
        stop();
        start(true);
        access(&Label::parse_canonical("//a:a").unwrap());
        assert_eq!(take().len(), 1);
        stop();
        scope_exit();
        assert!(take().is_empty());
    }
}
