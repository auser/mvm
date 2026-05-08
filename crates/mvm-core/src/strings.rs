/// Generate a random sandbox name.
pub fn generate_name() -> String {
    use rand::RngExt;
    let id: u32 = rand::rng().random();
    format!("msb-{id:08x}")
}
