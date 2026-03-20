fn main() {
    // No build-time steps needed for the root mvmctl binary.
    // Apple Container support uses XPC (via apple-container crate),
    // not Swift FFI, so no rpath or framework linking is required.
}
