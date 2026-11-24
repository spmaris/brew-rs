use super::limits::MAX_WARNINGS;

/// Bounded warning accumulator: at most [`MAX_WARNINGS`] messages plus one
/// summary line naming the number omitted.
pub(super) struct Warnings {
    messages: Vec<String>,
    omitted: usize,
}

impl Warnings {
    pub(super) fn new() -> Self {
        Self {
            messages: Vec::new(),
            omitted: 0,
        }
    }

    pub(super) fn push(&mut self, message: impl Into<String>) {
        if self.messages.len() == MAX_WARNINGS {
            self.omitted += 1;
        } else {
            self.messages.push(message.into());
        }
    }

    pub(super) fn finish(mut self) -> Vec<String> {
        if self.omitted > 0 {
            self.messages
                .push(format!("Inventory: {} more warnings omitted", self.omitted));
        }
        self.messages
    }
}
