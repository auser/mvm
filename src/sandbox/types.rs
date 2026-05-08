use std::path::PathBuf;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum DiskImageFormat {
    Raw,
    Qcow2,
    Oci,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum RootFsSource {
    Bind(PathBuf),
    Oci(String),
    DiskImage {
        path: PathBuf,
        format: DiskImageFormat,
        fstype: Option<String>,
    },
}

impl Default for RootFsSource {
    fn default() -> Self {
        RootFsSource::Oci(String::new())
    }
}

#[derive(Clone)]
pub enum VolumeMount {
    Bind {
        host: PathBuf,
        guest: String,
        readonly: bool,
    },
    Named {
        name: String,
        guest: String,
        readonly: bool,
    },
    Tmpfs {
        guest: String,
        size: Option<u32>,
        readonly: bool,
    },
}
