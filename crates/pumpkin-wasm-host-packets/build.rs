use std::{fs, path::Path};

fn main() {
    // Sharing is valid only while both public value schemas match. A future
    // divergent API must use separate values/conversions, never mutate v0.1.
    for name in ["java-packets.wit", "bedrock-packets.wit", "uuid.wit"] {
        #[expect(
            clippy::expect_used,
            reason = "Missing public schemas must stop the build"
        )]
        let read = |version: &str| {
            let path = Path::new("../pumpkin-plugin-wit").join(version).join(name);
            println!("cargo:rerun-if-changed={}", path.display());
            fs::read_to_string(path).expect("packet schema must be readable")
        };
        let v1 = read("v0.1");
        let v2 = read("v0.2").replace("pumpkin:plugin@0.2.0", "pumpkin:plugin@0.1.0");
        assert_eq!(v1, v2, "shared packet schema diverged: {name}");
        if name != "uuid.wit" {
            assert!(
                !v1.lines().any(|line| {
                    let line = line.trim_start();
                    !line.starts_with("//")
                        && line.split_whitespace().any(|token| {
                            token == "resource" || token == "func" || token.starts_with("func(")
                        })
                }),
                "packet facades require data-only interfaces: {name}"
            );
        }
    }
}
