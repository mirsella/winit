use std::collections::VecDeque;

#[derive(Debug, PartialEq, Eq)]
pub(super) enum CallbackAction<E> {
    Continue,
    Terminate(Vec<E>),
}

#[derive(Debug)]
pub(super) enum CallbackState<E> {
    Running(VecDeque<E>),
    Terminating(Vec<E>),
}

impl<E> CallbackState<E> {
    pub(super) fn new() -> Self {
        Self::Running(VecDeque::new())
    }

    pub(super) fn queue(&mut self, events: impl IntoIterator<Item = E>) {
        if let Self::Running(queued_events) = self {
            queued_events.extend(events);
        }
    }

    pub(super) fn request_termination(&mut self, events: Vec<E>) -> bool {
        if self.is_terminating() {
            return false;
        }

        *self = Self::Terminating(events);
        true
    }

    pub(super) fn is_terminating(&self) -> bool {
        matches!(self, Self::Terminating(_))
    }

    pub(super) fn take_action(&mut self, pending_events: &mut VecDeque<E>) -> CallbackAction<E> {
        match self {
            Self::Terminating(events) => {
                pending_events.clear();
                CallbackAction::Terminate(std::mem::take(events))
            },
            Self::Running(queued_events) => {
                pending_events.append(queued_events);
                CallbackAction::Continue
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;

    use super::{CallbackAction, CallbackState};

    #[test]
    fn no_queued_events_continue_without_pending_work() {
        let mut state = CallbackState::new();
        let mut pending = VecDeque::<i32>::new();

        assert_eq!(state.take_action(&mut pending), CallbackAction::Continue);
        assert!(pending.is_empty());
    }

    #[test]
    fn queued_events_append_in_fifo_order_across_batches() {
        let mut state = CallbackState::new();
        let mut pending = VecDeque::from([1, 2]);

        state.queue([3, 4]);
        assert_eq!(state.take_action(&mut pending), CallbackAction::Continue);
        assert_eq!(pending, VecDeque::from([1, 2, 3, 4]));

        state.queue([5, 6]);
        assert_eq!(state.take_action(&mut pending), CallbackAction::Continue);
        assert_eq!(pending, VecDeque::from([1, 2, 3, 4, 5, 6]));
    }

    #[test]
    fn termination_replaces_ordinary_queued_events() {
        let mut state = CallbackState::new();
        let mut pending = VecDeque::from([0]);
        state.queue([1, 2]);

        assert!(state.request_termination(vec![3, 4]));
        state.queue([5, 6]);

        assert!(state.is_terminating());
        assert_eq!(state.take_action(&mut pending), CallbackAction::Terminate(vec![3, 4]));
        assert!(pending.is_empty());
    }

    #[test]
    fn repeated_termination_keeps_the_first_request() {
        let mut state = CallbackState::new();
        let mut pending = VecDeque::new();

        assert!(state.request_termination(vec![1]));
        assert!(!state.request_termination(vec![2]));
        assert_eq!(state.take_action(&mut pending), CallbackAction::Terminate(vec![1]));
        assert!(state.is_terminating());
        assert!(!state.request_termination(vec![2]));
        state.queue([3]);
        assert_eq!(state.take_action(&mut pending), CallbackAction::Terminate(Vec::new()));
        assert!(pending.is_empty());
    }
}
