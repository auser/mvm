use serde::{Deserialize, Serialize};

use crate::sandbox::RootFsSource;

#[derive(Default, Debug, Clone, Serialize, Deserialize)]
pub struct SandboxConfig {
    pub name: String,

    #[serde(default)]
    pub image: RootFsSource,
}
