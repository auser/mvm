mod env;
pub use env::*;

pub(crate) trait BuilderBackend {
    fn prepare(&mut self, env: &dyn BuildEnvironment);
    fn build(&mut self, env: &dyn BuildEnvironment);
    fn extract_artifacts(&mut self, env: &dyn BuildEnvironment);
    fn cleanup(&mut self, env: &dyn BuildEnvironment);
}
// pub(crate) fn create_builder_output_disk(run_dir: &str, size_mib: u32) -> String {
//     format!("{}/build-out-{}m.ext4", run_dir, size_mib)
// }
