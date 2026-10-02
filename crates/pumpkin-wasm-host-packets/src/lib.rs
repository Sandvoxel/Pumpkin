mod bindings {
    wasmtime::component::bindgen!({
        path: "../pumpkin-plugin-wit/v0.1",
        interfaces: "
            import pumpkin:plugin/java-packets@0.1.0;
            import pumpkin:plugin/bedrock-packets@0.1.0;
        ",
    });
}

// Keep callable UUID bindings private; versioned hosts generate their own.
pub(crate) use bindings::pumpkin;
pub use bindings::pumpkin::plugin::{bedrock_packets, java_packets};
// Match the existing versioned hosts' allowance for generated conversions.
#[allow(clippy::unwrap_used)]
pub mod generated_packets;
