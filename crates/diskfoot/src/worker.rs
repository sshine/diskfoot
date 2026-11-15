//! Background subtree measurement with a per-`(path, options)` cache.
//!
//! The UI thread lists names and draws them immediately. The worker only walks
//! a directory whose row the user expanded, and reuses the cached result when
//! the same directory is collapsed and expanded again.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;

use crate::walk::{self, ChildSize, ErrorNote, Options};

#[derive(Debug, Clone)]
pub struct Job {
    pub dir: PathBuf,
    pub options: Options,
    /// Bumped by refresh and option changes; stale results are ignored.
    pub generation: u64,
}

#[derive(Debug, Clone)]
pub struct JobResult {
    pub dir: PathBuf,
    pub options: Options,
    pub generation: u64,
    pub children: Vec<ChildSize>,
    pub errors: Vec<ErrorNote>,
    pub skipped: u64,
}

enum Command {
    Measure(Job),
    Clear,
}

pub struct Worker {
    sender: Sender<Command>,
}

impl Worker {
    pub fn spawn() -> (Self, Receiver<JobResult>) {
        let (command_tx, command_rx) = mpsc::channel::<Command>();
        let (result_tx, result_rx) = mpsc::channel::<JobResult>();

        thread::spawn(move || {
            let mut cache: HashMap<(PathBuf, Options), walk::ChildrenResult> = HashMap::new();
            while let Ok(command) = command_rx.recv() {
                match command {
                    Command::Clear => cache.clear(),
                    Command::Measure(job) => {
                        let key = (job.dir.clone(), job.options);
                        let computed = match cache.get(&key) {
                            Some(cached) => cached.clone(),
                            None => {
                                let computed = walk::measure_children(&job.dir, job.options)
                                    .unwrap_or_else(|error| walk::ChildrenResult {
                                        children: Vec::new(),
                                        errors: vec![ErrorNote {
                                            path: job.dir.clone(),
                                            message: error.to_string(),
                                        }],
                                        skipped: 0,
                                    });
                                cache.insert(key, computed.clone());
                                computed
                            }
                        };
                        let _ = result_tx.send(JobResult {
                            dir: job.dir,
                            options: job.options,
                            generation: job.generation,
                            children: computed.children,
                            errors: computed.errors,
                            skipped: computed.skipped,
                        });
                    }
                }
            }
        });

        (Self { sender: command_tx }, result_rx)
    }

    pub fn measure(&self, dir: &std::path::Path, options: Options, generation: u64) {
        let _ = self.sender.send(Command::Measure(Job {
            dir: dir.to_path_buf(),
            options,
            generation,
        }));
    }

    pub fn clear(&self) {
        let _ = self.sender.send(Command::Clear);
    }
}
