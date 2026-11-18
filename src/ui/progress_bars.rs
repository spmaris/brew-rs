use indicatif::{ProgressBar, ProgressStyle, MultiProgress};
use std::time::Duration;

pub struct ProgressManager {
    multi: MultiProgress,
    show_progress: bool,
}

impl ProgressManager {
    pub fn new(show_progress: bool) -> Self {
        Self {
            multi: MultiProgress::new(),
            show_progress,
        }
    }

    pub fn create_spinner(&self, message: &str) -> Option<ProgressBar> {
        if !self.show_progress {
            return None;
        }

        let pb = self.multi.add(ProgressBar::new_spinner());
        pb.set_style(
            ProgressStyle::default_spinner()
                .template("{spinner:.cyan} {msg}")
                .unwrap()
                .tick_chars("⠁⠂⠄⡀⢀⠠⠐⠈ "),
        );
        pb.set_message(message.to_string());
        pb.enable_steady_tick(Duration::from_millis(100));
        
        Some(pb)
    }
    
    pub fn finish_with_message(&self, pb: Option<ProgressBar>, message: &str) {
        if let Some(pb) = pb {
            pb.finish_with_message(message.to_string());
        }
    }
}