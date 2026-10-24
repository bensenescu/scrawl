//! Loading a Metal shader library: the `.metallib` `build.rs` compiled, or, when the build
//! machine had no Metal toolchain (the library is empty), the shader's source compiled by the
//! GPU driver at launch. The source route is for development builds without Xcode; releases
//! always ship compiled libraries.

/// `metallib` is the compiled library (empty when it couldn't be built) and `source` the shader
/// it was built from.
pub fn library(
    device: &metal::DeviceRef,
    name: &str,
    metallib: &[u8],
    source: &str,
) -> Result<metal::Library, String> {
    if !metallib.is_empty() {
        return device
            .new_library_with_data(metallib)
            .map_err(|e| format!("Could not load {name}.metallib: {e}"));
    }
    crate::capture::scrawl_log(format!(
        "{name}: no compiled metallib in this build; compiling the shader from source"
    ));
    device
        .new_library_with_source(source, &metal::CompileOptions::new())
        .map_err(|e| format!("Could not compile {name}.metal: {e}"))
}
