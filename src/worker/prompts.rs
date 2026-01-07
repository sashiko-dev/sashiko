use anyhow::Result;
use std::path::PathBuf;

pub struct PromptRegistry {
    base_dir: PathBuf,
}

impl PromptRegistry {
    pub fn new(base_dir: PathBuf) -> Self {
        Self { base_dir }
    }

    pub fn get_base_dir(&self) -> &PathBuf {
        &self.base_dir
    }

    pub async fn get_system_prompt(&self) -> Result<String> {
        let identity = "You're an expert Linux kernel developer and maintainer with deep knowledge of Linux, Operating Systems, modern hardware and Linux community standards and processes.";
        Ok(identity.to_string())
    }
}
